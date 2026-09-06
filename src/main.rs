//! IRA duplex voice loop prototype.
//!
//! Idle --wake word--> Listening --silence--> Holding --barge-in--> Listening
//!
//! The only claim this prototype makes is that the loop *feels* right: fast
//! enough to talk to, interruptible mid-sentence, and it does not cut you off
//! when you pause to think. Tools, memory, routing and the UI come after.
//!
//! Every turn emits one `turn` log line with its stage timings, because "feels
//! right" is a number or it is an opinion. See `metrics.rs`.
//!
//! ponytail: no acoustic echo cancellation. Wear headphones. Without AEC the mic
//! hears IRA's own speech and barge-in fires on her own voice, so speaker mode
//! needs `webrtc-audio-processing` wired into audio.rs before it is usable.

mod audio;
mod config;
mod doctor;
mod llm;
mod mcp;
mod metrics;
mod stt;
mod tool;
mod tts;
mod ui;
mod vad;
mod wake;

use anyhow::{Context, Result};
use metrics::Timings;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Silence after speech that ends the user's turn.
const ENDPOINT_MS: u64 = 700;
/// A wake word with no speech after it was a false trigger.
const NO_SPEECH_TIMEOUT_MS: u64 = 3_000;
/// How long IRA waits for a spoken yes or no before treating silence as no.
/// Generous, because being asked a question and then cut off is worse than
/// waiting: the alternative to patience here is doing something unasked.
const CONFIRM_TIMEOUT_MS: u64 = 6_000;
/// How long the floor stays open after a reply, so a follow-up needs no wake
/// word. Shorter than the post-wake timeout: after a wake word the user has
/// announced they are about to speak and deserves patience, whereas holding the
/// floor open for three seconds after every single reply just makes IRA feel
/// like it is waiting for something.
const FOLLOW_UP_MS: u64 = 2_000;
/// Hard cap on one utterance.
const MAX_UTTERANCE_MS: u64 = 20_000;
/// Speech this long while IRA holds the floor counts as an interruption.
/// Too low and a cough stops her; too high and interrupting feels laggy.
const BARGE_IN_MS: u64 = 250;
/// Ignore barge-in for this long after IRA's *first sound*, so the tail of a
/// reply's own first word cannot interrupt it.
///
/// Measured from first audio out rather than from the start of the turn: STT and
/// the model routinely spend longer than this before IRA has said anything, so
/// timing it from the turn start left the window already expired and protecting
/// nothing.
const BARGE_IN_GRACE_MS: u64 = 300;

/// The three waits are meant to differ, and in this order: a pending question
/// gets the most patience, a wake word next, a follow-up the least. Checked at
/// compile time because reordering them is a silent change in how IRA feels,
/// with nothing to fail.
const _: () = assert!(FOLLOW_UP_MS < NO_SPEECH_TIMEOUT_MS);
const _: () = assert!(NO_SPEECH_TIMEOUT_MS < CONFIRM_TIMEOUT_MS);

const PRE_ROLL: usize = audio::SR as usize; // 1 s
/// Audio kept from before the wake word fired, so a fast "IRA, what time is it"
/// does not lose the "what".
const PRE_ROLL_KEEP: usize = audio::SR as usize * 2 / 5; // 400 ms

/// Fixed phrases, not model output: an LLM failure must not need the LLM to
/// report itself, and a failure the user cannot hear is the same as a crash.
const SAY_STT_FAILED: &str = "I didn't catch that.";
const SAY_LLM_FAILED: &str = "I'm having trouble thinking right now.";
const SAY_CANCELLED: &str = "Cancelled.";

#[derive(Debug, PartialEq)]
enum State {
    Idle,
    Listening,
    /// Thinking and speaking are one state: IRA has the floor, barge-in armed.
    Holding,
    /// A tool wants to change something and IRA has asked whether to. A state
    /// rather than a helper because a refusal must end the turn rather than
    /// start a new one, and because only yes or no is an answer here.
    Confirming,
}

/// Which stage failed, so the loop can say the right thing without a round trip.
enum Fail {
    Stt,
    Llm,
}

enum Turn {
    Heard(String),
    /// What the user said in answer to a confirmation question.
    Confirmed(String),
    Done { user: String, reply: String },
    /// Transcription returned nothing -- almost always noise after a false wake.
    Empty,
    Failed(Fail),
}

/// What Listening should do with the audio it has seen so far.
#[derive(Debug, PartialEq)]
enum Next {
    /// Keep buffering.
    Wait,
    /// The user's turn is over. Answer it.
    Answer,
    /// Nobody spoke. Give the floor back without a sound.
    GiveUp,
}

/// Decides when to stop listening.
///
/// Extracted from the loop because the three exits interact: the no-speech
/// deadline must stop applying the instant speech is heard, or a slow speaker
/// gets cut off. The deadline is a parameter because three states wait for
/// speech with different amounts of patience -- after a wake word, during a
/// follow-up window, and while a confirmation is pending.
fn listening_next(
    heard_speech: bool,
    silence_ms: u64,
    utterance_ms: u64,
    no_speech_deadline_ms: u64,
) -> Next {
    if !heard_speech {
        return if utterance_ms >= no_speech_deadline_ms {
            Next::GiveUp
        } else {
            Next::Wait
        };
    }
    if silence_ms >= ENDPOINT_MS || utterance_ms >= MAX_UTTERANCE_MS {
        Next::Answer
    } else {
        Next::Wait
    }
}

/// Whether this frame should take the floor back from IRA.
///
/// The talk control always interrupts: it is a person pressing a button, and
/// there is nothing to second-guess. Voice barge-in is conditional, because
/// without echo cancellation a speaker feeds IRA's own reply back into the
/// microphone and every sentence would interrupt itself.
fn should_interrupt(ptt: bool, past_grace: bool, barge_ms: u64, talk: bool) -> bool {
    if talk {
        return true;
    }
    !ptt && past_grace && barge_ms >= BARGE_IN_MS
}

/// Moves to a new state and tells the screen.
///
/// One function so the screen cannot drift out of step with the loop: there is
/// no way to change state without saying so.
fn go(state: &mut State, next: State, ui: &ui::Ui) {
    ui.send(ui::Event::State {
        name: match next {
            State::Idle => "idle",
            State::Listening => "listening",
            State::Holding => "holding",
            State::Confirming => "confirming",
        },
    });
    *state = next;
}

/// Refuses a pending confirmation and ends the turn that asked.
///
/// Every path out of Confirming that is not an explicit yes comes through here,
/// so there is exactly one place where "not consent" is decided.
fn deny(pending: &mut Option<oneshot::Sender<bool>>, cancel: &CancellationToken) {
    if let Some(tx) = pending.take() {
        let _ = tx.send(false);
    }
    cancel.cancel();
}

/// A sentence may only be spoken while the turn that produced it still holds
/// the floor.
///
/// The token is the turn's own, carried alongside the sentence. Checking the
/// loop's current token instead let a sentence buffered from an interrupted turn
/// be vouched for by the *next* turn's fresh token, and spoken after the user
/// had already cut it off.
fn should_speak(tok: &CancellationToken, state: &State) -> bool {
    *state == State::Holding && !tok.is_cancelled()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // Must match the crate name -- tracing targets are module
                // paths, so a rename here silently turns off all logging.
                .unwrap_or_else(|_| "ira=info".into()),
        )
        .init();

    let models = std::env::var("IRA_MODELS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("models"));
    let wakeword = std::env::var("IRA_WAKEWORD").unwrap_or_else(|_| "hey_jarvis_v0.1.onnx".into());
    let piper = PathBuf::from(std::env::var("IRA_PIPER").unwrap_or_else(|_| "piper/piper.exe".into()));
    let voice = models.join(std::env::var("IRA_VOICE").unwrap_or_else(|_| "en_US-amy-medium.onnx".into()));

    let checks = doctor::paths(&models, &wakeword, &voice, &piper);
    if std::env::args().nth(1).as_deref() == Some("doctor") {
        let report = doctor::all(&checks).await;
        std::process::exit(doctor::report(&report));
    }
    // Anything knowable now must fail now. A missing key that surfaces as
    // silence three seconds into the first sentence looks like a broken product
    // rather than an unconfigured one.
    if let Some(problem) = doctor::first_fatal(&doctor::files_and_keys(&checks)) {
        anyhow::bail!("{problem}\n\nrun `ira doctor` for the full report");
    }

    let cfg = config::load()?;

    let mut wake = wake::WakeWord::new(&models, &wakeword, 0.5)
        .with_context(|| format!("load wake models from {}", models.display()))?;
    let mut vad = vad::Vad::new(&models.join("silero_vad.onnx"), 0.5).context("load silero vad")?;
    let mut tts = tts::Tts::new(&piper, &voice).context("start piper")?;
    let mut mic = audio::Mic::open().context("open microphone")?;

    let client = reqwest::Client::new();
    let (turn_tx, mut turn_rx) = mpsc::channel::<Turn>(16);
    // Sentences flow straight from the LLM stream to the main loop, which owns
    // the TTS handle. Speaking starts before the model finishes writing.
    let (speech_tx, mut speech_rx) = mpsc::channel::<(String, CancellationToken)>(32);

    // Tools. The clock is the only built-in; everything else arrives over MCP
    // as configuration rather than code.
    let (ui, mut talk_rx) = ui::Ui::start().await;
    // Without acoustic echo cancellation the microphone hears the speaker, so
    // voice-triggered barge-in fires on IRA's own reply. Disarming it makes
    // speaker mode usable at the cost of hands-free interruption; the talk
    // control interrupts instead.
    let ptt = std::env::var("IRA_PTT").is_ok();
    if ptt {
        tracing::info!("press-to-talk: barge-in is the talk control, not your voice");
    }
    let (confirm_tx, mut confirm_rx) = mpsc::channel::<tool::Confirm>(4);
    let host = {
        let mut h = tool::Host::new(confirm_tx, ui.clone());
        h.add(Arc::new(tool::Clock));
        for t in mcp::connect_all(&cfg.mcp.server).await {
            h.add(t);
        }
        Arc::new(h)
    };
    tracing::info!(tools = host.specs().len(), "tool registry");

    let stt_backend = metrics::stt_backend();
    let llm_model = std::env::var("IRA_LLM_MODEL").unwrap_or_else(|_| llm::MODEL.to_string());

    // openWakeWord is trained on real speech and does not fire on synthesised
    // audio, so a Piper-generated corpus never gets past Idle. NFR-1 measures
    // endpoint-to-first-audio and does not involve the wake word anyway, so
    // latency replays start with the floor already open. Wake detection is
    // measured separately, against real recordings.
    let skip_wake = std::env::var("IRA_SKIP_WAKE").is_ok();
    let mut state = if skip_wake {
        tracing::info!("skipping wake word -- listening immediately");
        State::Listening
    } else {
        State::Idle
    };
    let mut history: Vec<(String, String)> = Vec::new();
    let mut cancel = CancellationToken::new();

    let mut pre_roll: VecDeque<f32> = VecDeque::with_capacity(PRE_ROLL);
    let mut utterance: Vec<f32> = Vec::new();
    let mut heard_speech = false;
    let mut silence_ms = 0u64;
    let mut utterance_ms = 0u64;
    let mut barge_ms = 0u64;
    let mut llm_running = false;
    // Whether this Listening was entered without a wake word, by IRA holding the
    // floor open after a reply.
    let mut follow_up = false;
    // The tool waiting on a spoken yes or no, and the question it asked.
    let mut pending_confirm: Option<oneshot::Sender<bool>> = None;
    let mut confirm_question = String::new();
    let mut confirm_reasked = false;
    let mut confirm_stt_running = false;
    // Set by the talk control, consumed by the next audio frame. At 80 ms a
    // frame that is well inside the interruption budget, and it means the
    // control reuses the state machine rather than duplicating it.
    let mut talk = false;
    let mut ducked = false;
    // Whether this turn got any words out. A stream that breaks after two
    // sentences must not append an apology to them.
    let mut spoke_any = false;

    let mut turn_id = 0u64;
    let mut wake_ms = 0u64;
    let mut wake_at: Option<Instant> = if skip_wake { Some(Instant::now()) } else { None };
    let mut turn: Option<metrics::Turn> = None;
    // The TTS clock reading when IRA took the floor, so first-audio can be
    // expressed relative to the endpoint.
    let mut hold_base_ms = 0u64;

    tracing::info!(stt_backend, %llm_model, "ready -- say the wake word; ctrl-c to quit");

    loop {
        tokio::select! {
            frame = mic.rx.recv() => {
                // A closed channel means a replayed file ran out. Live capture
                // never ends, so this is the end of a test run.
                let Some(frame) = frame else {
                    tracing::info!("audio input ended");
                    // Flush an in-flight turn so a replay always produces its
                    // line, even when the file ran out mid-answer. A benchmark
                    // reading nothing cannot tell "fast" from "never started".
                    log_turn(&mut turn, &tts, hold_base_ms, false, stt_backend, &llm_model, &ui);
                    break;
                };
                let frame_ms = (frame.len() as u64 * 1000) / audio::SR as u64;

                pre_roll.extend(frame.iter().copied());
                while pre_roll.len() > PRE_ROLL {
                    pre_roll.pop_front();
                }

                match state {
                    State::Idle => {
                        let woke = wake.push(&frame)?;
                        if woke.is_some() || talk {
                            let score = woke.unwrap_or(0.0);
                            let fired = Instant::now();
                            tts.chirp();
                            wake_ms = fired.elapsed().as_millis() as u64;
                            wake_at = Some(fired);
                            tracing::info!(score, "wake");
                            vad.reset();
                            utterance = tail(&pre_roll, PRE_ROLL_KEEP);
                            heard_speech = false;
                            silence_ms = 0;
                            utterance_ms = 0;
                            follow_up = false;
                            go(&mut state, State::Listening, &ui);
                        }
                    }

                    State::Listening => {
                        utterance.extend_from_slice(&frame);
                        utterance_ms += frame_ms;
                        for speech in vad.push(&frame)? {
                            if speech {
                                heard_speech = true;
                                silence_ms = 0;
                            } else if heard_speech {
                                silence_ms += vad::CHUNK_MS;
                            }
                        }

                        let deadline = if follow_up { FOLLOW_UP_MS } else { NO_SPEECH_TIMEOUT_MS };
                        match listening_next(heard_speech, silence_ms, utterance_ms, deadline) {
                            Next::Wait => {}
                            Next::GiveUp => {
                                if follow_up {
                                    tracing::info!("no follow-up, back to idle");
                                } else {
                                    tracing::info!("no speech after wake, back to idle");
                                }
                                vad.reset();
                                go(&mut state, State::Idle, &ui);
                            }
                            Next::Answer => {
                                cancel = CancellationToken::new();
                                llm_running = true;
                                spoke_any = false;
                                barge_ms = 0;
                                vad.reset();
                                turn_id += 1;
                                let listen_ms = wake_at
                                    .map(|w| w.elapsed().as_millis() as u64)
                                    .unwrap_or(0);
                                let t = metrics::Turn::start(turn_id, wake_ms, listen_ms);
                                let timings = t.timings.clone();
                                turn = Some(t);
                                hold_base_ms = tts.elapsed_ms();
                                tts.begin_turn();
                                go(&mut state, State::Holding, &ui);
                                spawn_turn(
                                    client.clone(),
                                    std::mem::take(&mut utterance),
                                    history.clone(),
                                    turn_tx.clone(),
                                    speech_tx.clone(),
                                    cancel.clone(),
                                    timings,
                                    host.clone(),
                                    // Checked as the turn starts: IRA may only
                                    // promise a screen someone is looking at.
                                    ui.watchers() > 0,
                                );
                            }
                        }
                    }

                    State::Confirming => {
                        utterance.extend_from_slice(&frame);
                        utterance_ms += frame_ms;
                        for speech in vad.push(&frame)? {
                            if speech {
                                heard_speech = true;
                                silence_ms = 0;
                            } else if heard_speech {
                                silence_ms += vad::CHUNK_MS;
                            }
                        }

                        match listening_next(
                            heard_speech,
                            silence_ms,
                            utterance_ms,
                            CONFIRM_TIMEOUT_MS,
                        ) {
                            Next::Wait => {}
                            Next::GiveUp => {
                                // Silence is not consent, and saying so out loud
                                // would be one more thing to talk over.
                                tracing::info!("no answer to the confirmation");
                                deny(&mut pending_confirm, &cancel);
                                vad.reset();
                                go(&mut state, State::Idle, &ui);
                            }
                            Next::Answer if !confirm_stt_running => {
                                confirm_stt_running = true;
                                spawn_confirm(
                                    client.clone(),
                                    std::mem::take(&mut utterance),
                                    turn_tx.clone(),
                                );
                                heard_speech = false;
                                silence_ms = 0;
                                utterance_ms = 0;
                            }
                            Next::Answer => {}
                        }
                    }

                    State::Holding => {
                        for speech in vad.push(&frame)? {
                            if speech {
                                barge_ms += vad::CHUNK_MS;
                            } else {
                                barge_ms = 0;
                            }
                        }

                        // Nothing to protect until IRA has actually made a
                        // sound; before that a confirmed 250 ms of speech is the
                        // user changing their mind, and should land.
                        let past_grace = match tts.first_audio_ms() {
                            Some(a) => tts.elapsed_ms().saturating_sub(a) > BARGE_IN_GRACE_MS,
                            None => true,
                        };

                        // Duck at the first hint, restore if it comes to
                        // nothing. Not while press-to-talk is on: there the
                        // speech is probably IRA's own, coming back in.
                        if !ptt {
                            if barge_ms > 0 && !ducked {
                                tts.duck();
                                ducked = true;
                            } else if barge_ms == 0 && ducked {
                                tts.unduck();
                                ducked = false;
                            }
                        }

                        if should_interrupt(ptt, past_grace, barge_ms, talk) {
                            tracing::info!(by = if talk { "talk" } else { "voice" }, "barge-in");
                            ducked = false;
                            cancel.cancel();
                            llm_running = false;
                            log_turn(&mut turn, &tts, hold_base_ms, true, stt_backend, &llm_model, &ui);
                            tts.interrupt()?;
                            vad.reset();
                            // Seed the new turn from the pre-roll so the words
                            // that did the interrupting are part of it.
                            utterance = tail(&pre_roll, PRE_ROLL);
                            heard_speech = true;
                            silence_ms = 0;
                            utterance_ms = 0;
                            barge_ms = 0;
                            follow_up = false;
                            // The interrupted turn's wake is no longer the start
                            // of anything; this turn began when they cut in.
                            wake_ms = 0;
                            wake_at = Some(Instant::now());
                            go(&mut state, State::Listening, &ui);
                        } else if !llm_running && tts.idle() {
                            if ducked {
                                tts.unduck();
                                ducked = false;
                            }
                            log_turn(&mut turn, &tts, hold_base_ms, false, stt_backend, &llm_model, &ui);
                            // Hold the floor open briefly so a reply can be
                            // answered without a wake word. This also covers the
                            // failure phrases: after "I didn't catch that", the
                            // user can simply say it again.
                            tracing::info!("floor open for a follow-up");
                            vad.reset();
                            // The drain check waits for quiet, so the user may
                            // already have started. Seed from the pre-roll or
                            // their first word is lost.
                            utterance = tail(&pre_roll, PRE_ROLL_KEEP);
                            heard_speech = false;
                            silence_ms = 0;
                            utterance_ms = 0;
                            follow_up = true;
                            // No new wake word, so the turn line measures from
                            // the moment the floor opened.
                            wake_ms = 0;
                            wake_at = Some(Instant::now());
                            go(&mut state, State::Listening, &ui);
                        }
                    }
                }
                // Consumed either way: a press must not surface two states later.
                talk = false;
            }

            Some(_) = talk_rx.recv() => {
                tracing::info!("talk");
                talk = true;
            }

            Some(req) = confirm_rx.recv() => {
                tracing::info!(question = %req.question, "confirming");
                ui.send(ui::Event::Confirm { question: req.question.clone() });
                // Asked directly rather than through the sentence channel: this
                // is IRA's own question, not part of the model's reply.
                if let Err(e) = tts.say(&req.question) {
                    tracing::error!(?e, "tts");
                }
                confirm_question = req.question;
                pending_confirm = Some(req.reply);
                confirm_reasked = false;
                confirm_stt_running = false;
                vad.reset();
                utterance.clear();
                heard_speech = false;
                silence_ms = 0;
                utterance_ms = 0;
                go(&mut state, State::Confirming, &ui);
            }

            Some((sentence, tok)) = speech_rx.recv() => {
                if should_speak(&tok, &state) {
                    match tts.say(&sentence) {
                        Ok(()) => spoke_any = true,
                        Err(e) => {
                            // Piper is gone, so it cannot apologise for itself.
                            // Respawn first, then make a sound the user can hear.
                            tracing::error!(?e, "tts");
                            if let Err(e) = tts.interrupt() {
                                tracing::error!(?e, "tts respawn");
                            }
                            tts.error_tone();
                        }
                    }
                }
            }

            Some(event) = turn_rx.recv() => {
                match event {
                    Turn::Heard(text) => {
                        tracing::info!(user = %text, "heard");
                        ui.send(ui::Event::Heard { text });
                    }
                    Turn::Done { user, reply } => {
                        llm_running = false;
                        if !reply.is_empty() {
                            tracing::info!(ira = %reply, "reply");
                            ui.send(ui::Event::Reply { text: reply.clone() });
                            history.push((user, reply));
                            // ponytail: fixed-window history. kortex-memory
                            // replaces this with real summarisation and recall.
                            if history.len() > 8 {
                                history.remove(0);
                            }
                        }
                    }
                    Turn::Confirmed(text) => {
                        confirm_stt_running = false;
                        tracing::info!(answer = %text, "confirmation answer");
                        match tool::yes_no(&text) {
                            Some(true) => {
                                ui.send(ui::Event::Answered { yes: true });
                                if let Some(tx) = pending_confirm.take() {
                                    let _ = tx.send(true);
                                }
                                // The turn resumes where it left off.
                                go(&mut state, State::Holding, &ui);
                            }
                            Some(false) => {
                                ui.send(ui::Event::Answered { yes: false });
                                let _ = tts.say(SAY_CANCELLED);
                                deny(&mut pending_confirm, &cancel);
                                // Back to Holding so "Cancelled." is actually
                                // heard; the cancelled turn then drains and the
                                // follow-up window opens as usual.
                                go(&mut state, State::Holding, &ui);
                            }
                            None if !confirm_reasked => {
                                // Ambiguity gets one more chance, then fails
                                // closed. Guessing at "maybe" is how a tool runs
                                // that nobody agreed to.
                                confirm_reasked = true;
                                let _ = tts.say(&confirm_question);
                                heard_speech = false;
                                silence_ms = 0;
                                utterance_ms = 0;
                            }
                            None => {
                                let _ = tts.say(SAY_CANCELLED);
                                deny(&mut pending_confirm, &cancel);
                                go(&mut state, State::Holding, &ui);
                            }
                        }
                    }
                    Turn::Empty => {
                        llm_running = false;
                        tracing::info!("nothing transcribed");
                        // A sentence here would be worse than a sound: this is
                        // usually a false wake, and the user never spoke.
                        tts.error_tone();
                    }
                    Turn::Failed(what) => {
                        llm_running = false;
                        match what {
                            Fail::Stt => {
                                tracing::error!("stt failed");
                                ui.send(ui::Event::Failed { what: "transcription".into() });
                                let _ = tts.say(SAY_STT_FAILED);
                            }
                            Fail::Llm => {
                                tracing::error!("llm failed");
                                ui.send(ui::Event::Failed { what: "the model".into() });
                                // Half a reply plus an apology is worse than
                                // half a reply. Only speak if nothing was said.
                                if !spoke_any {
                                    let _ = tts.say(SAY_LLM_FAILED);
                                }
                            }
                        }
                    }
                }
            }

            _ = tokio::signal::ctrl_c() => break,
        }
    }

    Ok(())
}

/// Emits the turn line and clears it, so a turn is never logged twice.
#[allow(clippy::too_many_arguments)]
fn log_turn(
    turn: &mut Option<metrics::Turn>,
    tts: &tts::Tts,
    hold_base_ms: u64,
    barged: bool,
    backend: &str,
    model: &str,
    ui: &ui::Ui,
) {
    if let Some(t) = turn.take() {
        let first_audio = tts
            .first_audio_ms()
            .map(|a| a.saturating_sub(hold_base_ms));
        t.log(first_audio, barged, backend, model);
        ui.send(t.event(first_audio, barged));
    }
}

/// Transcribes a yes-or-no answer.
///
/// Deliberately not a full turn: no history, no model, no tools. A confirmation
/// is only ever read for consent, and routing it through the model would give
/// the thing being confirmed a chance to talk its way past the question.
fn spawn_confirm(client: reqwest::Client, audio: Vec<f32>, out: mpsc::Sender<Turn>) {
    tokio::spawn(async move {
        let text = match stt::transcribe(&client, &audio, audio::SR).await {
            Ok(t) => t,
            Err(e) => {
                // An unreadable answer is not a yes.
                tracing::error!(?e, "stt during confirmation");
                String::new()
            }
        };
        let _ = out.send(Turn::Confirmed(text)).await;
    });
}

/// Last `n` samples of the ring buffer, or all of it if it holds fewer.
fn tail(buf: &VecDeque<f32>, n: usize) -> Vec<f32> {
    buf.iter().skip(buf.len().saturating_sub(n)).copied().collect()
}

#[allow(clippy::too_many_arguments)]
fn spawn_turn(
    client: reqwest::Client,
    audio: Vec<f32>,
    history: Vec<(String, String)>,
    out: mpsc::Sender<Turn>,
    speech: mpsc::Sender<(String, CancellationToken)>,
    cancel: CancellationToken,
    timings: Arc<Timings>,
    host: Arc<tool::Host>,
    screen: bool,
) {
    let cancelled = cancel.clone();
    tokio::spawn(async move {
        let asked = Instant::now();
        let result = stt::transcribe(&client, &audio, audio::SR).await;
        // Recorded even on failure: a slow failure and a fast one are different
        // problems, and the log line is the only place that shows which.
        Timings::set(&timings.stt_ms, asked.elapsed().as_millis() as u64);

        let text = match result {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(?e, "stt");
                let _ = out.send(Turn::Failed(Fail::Stt)).await;
                return;
            }
        };
        if text.is_empty() {
            let _ = out.send(Turn::Empty).await;
            return;
        }
        if cancel.is_cancelled() {
            let _ = out.send(Turn::Done { user: text, reply: String::new() }).await;
            return;
        }
        let _ = out.send(Turn::Heard(text.clone())).await;

        match llm::stream(&client, &history, &text, speech, cancel, &timings, &host, screen)
            .await
        {
            // A cancelled stream still returns its partial text; that half a
            // sentence must not enter history as if IRA had said it.
            Ok(reply) if !cancelled.is_cancelled() => {
                let _ = out.send(Turn::Done { user: text, reply }).await;
            }
            Ok(_) => {
                let _ = out.send(Turn::Done { user: text, reply: String::new() }).await;
            }
            Err(e) => {
                tracing::error!(?e, "llm");
                let _ = out.send(Turn::Failed(Fail::Llm)).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The barge-in race, as a test.
    ///
    /// Turn A is interrupted, so its token is cancelled. Turn B starts with a
    /// fresh token. A sentence from A that was still sitting in the channel
    /// must not be spoken just because B now holds the floor.
    #[test]
    fn a_sentence_from_an_interrupted_turn_is_never_spoken() {
        let turn_a = CancellationToken::new();
        turn_a.cancel(); // the user barged in

        let turn_b = CancellationToken::new(); // the turn they barged in with

        assert!(
            !should_speak(&turn_a, &State::Holding),
            "a cancelled turn's sentence was spoken during the next turn"
        );
        assert!(
            should_speak(&turn_b, &State::Holding),
            "the live turn must still be able to speak"
        );
    }

    /// A follow-up gets less patience than a wake word, and the difference has
    /// to be exactly at the two deadlines.
    #[test]
    fn each_state_waits_exactly_as_long_as_it_should() {
        // Just inside each deadline: still waiting.
        assert_eq!(listening_next(false, 0, FOLLOW_UP_MS - 1, FOLLOW_UP_MS), Next::Wait);
        assert_eq!(
            listening_next(false, 0, NO_SPEECH_TIMEOUT_MS - 1, NO_SPEECH_TIMEOUT_MS),
            Next::Wait
        );
        assert_eq!(
            listening_next(false, 0, CONFIRM_TIMEOUT_MS - 1, CONFIRM_TIMEOUT_MS),
            Next::Wait
        );

        // At the deadline: give the floor back.
        assert_eq!(listening_next(false, 0, FOLLOW_UP_MS, FOLLOW_UP_MS), Next::GiveUp);
        assert_eq!(
            listening_next(false, 0, NO_SPEECH_TIMEOUT_MS, NO_SPEECH_TIMEOUT_MS),
            Next::GiveUp
        );
        assert_eq!(
            listening_next(false, 0, CONFIRM_TIMEOUT_MS, CONFIRM_TIMEOUT_MS),
            Next::GiveUp
        );

        // A wake word's patience must not shrink to the follow-up window's.
        assert_eq!(listening_next(false, 0, FOLLOW_UP_MS, NO_SPEECH_TIMEOUT_MS), Next::Wait);
    }

    /// Once someone is talking, no deadline may cut them off. Only silence or
    /// the hard cap ends a turn.
    #[test]
    fn a_speaker_is_never_cut_off_by_the_no_speech_deadline() {
        // Long past every deadline, mid-sentence, brief pause.
        assert_eq!(listening_next(true, 200, 30_000, FOLLOW_UP_MS), Next::Answer);
        assert_eq!(listening_next(true, 0, 10_000, FOLLOW_UP_MS), Next::Wait);
        assert_eq!(listening_next(true, 0, 10_000, CONFIRM_TIMEOUT_MS), Next::Wait);

        // Endpoint, exactly at the boundary.
        assert_eq!(
            listening_next(true, ENDPOINT_MS - 1, 5_000, NO_SPEECH_TIMEOUT_MS),
            Next::Wait
        );
        assert_eq!(
            listening_next(true, ENDPOINT_MS, 5_000, NO_SPEECH_TIMEOUT_MS),
            Next::Answer
        );

        // The hard cap ends a monologue even with no pause at all.
        assert_eq!(
            listening_next(true, 0, MAX_UTTERANCE_MS, NO_SPEECH_TIMEOUT_MS),
            Next::Answer
        );
    }

    /// Press-to-talk exists so speakers work without echo cancellation. If
    /// voice could still interrupt, it would not solve anything: IRA's own
    /// reply is what the microphone is hearing.
    #[test]
    fn press_to_talk_disarms_the_voice_but_not_the_button() {
        // Voice, clearly past every threshold.
        assert!(should_interrupt(false, true, BARGE_IN_MS, false));
        assert!(!should_interrupt(true, true, BARGE_IN_MS, false));
        assert!(!should_interrupt(true, true, 10_000, false));

        // The button works in both modes, and does not wait for the grace
        // window: a person pressing it has already decided.
        assert!(should_interrupt(true, false, 0, true));
        assert!(should_interrupt(false, false, 0, true));
    }

    /// Voice barge-in still needs a confirmed run of speech and the grace
    /// window, or a cough during the first syllable stops the reply.
    #[test]
    fn a_brief_noise_does_not_interrupt() {
        assert!(!should_interrupt(false, true, BARGE_IN_MS - 1, false));
        assert!(!should_interrupt(false, false, BARGE_IN_MS, false));
        assert!(!should_interrupt(false, true, 0, false));
    }

    /// Sentences arriving while the user has the floor are dropped, whatever
    /// their token says.
    #[test]
    fn nothing_is_spoken_unless_ira_holds_the_floor() {
        let live = CancellationToken::new();
        assert!(!should_speak(&live, &State::Listening));
        assert!(!should_speak(&live, &State::Idle));
        assert!(should_speak(&live, &State::Holding));
    }
}
