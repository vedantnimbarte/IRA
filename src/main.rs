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
mod doctor;
mod llm;
mod metrics;
mod stt;
mod tts;
mod vad;
mod wake;

use anyhow::{Context, Result};
use metrics::Timings;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Silence after speech that ends the user's turn.
const ENDPOINT_MS: u64 = 700;
/// A wake word with no speech after it was a false trigger.
const NO_SPEECH_TIMEOUT_MS: u64 = 3_000;
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

const PRE_ROLL: usize = audio::SR as usize; // 1 s
/// Audio kept from before the wake word fired, so a fast "IRA, what time is it"
/// does not lose the "what".
const PRE_ROLL_KEEP: usize = audio::SR as usize * 2 / 5; // 400 ms

/// Fixed phrases, not model output: an LLM failure must not need the LLM to
/// report itself, and a failure the user cannot hear is the same as a crash.
const SAY_STT_FAILED: &str = "I didn't catch that.";
const SAY_LLM_FAILED: &str = "I'm having trouble thinking right now.";

#[derive(Debug, PartialEq)]
enum State {
    Idle,
    Listening,
    /// Thinking and speaking are one state: IRA has the floor, barge-in armed.
    Holding,
}

/// Which stage failed, so the loop can say the right thing without a round trip.
enum Fail {
    Stt,
    Llm,
}

enum Turn {
    Heard(String),
    Done { user: String, reply: String },
    /// Transcription returned nothing -- almost always noise after a false wake.
    Empty,
    Failed(Fail),
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
                    log_turn(&mut turn, &tts, hold_base_ms, false, stt_backend, &llm_model);
                    break;
                };
                let frame_ms = (frame.len() as u64 * 1000) / audio::SR as u64;

                pre_roll.extend(frame.iter().copied());
                while pre_roll.len() > PRE_ROLL {
                    pre_roll.pop_front();
                }

                match state {
                    State::Idle => {
                        if let Some(score) = wake.push(&frame)? {
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
                            state = State::Listening;
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

                        if !heard_speech && utterance_ms >= NO_SPEECH_TIMEOUT_MS {
                            tracing::info!("no speech after wake, back to idle");
                            state = State::Idle;
                        } else if (heard_speech && silence_ms >= ENDPOINT_MS)
                            || utterance_ms >= MAX_UTTERANCE_MS
                        {
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
                            state = State::Holding;
                            spawn_turn(
                                client.clone(),
                                std::mem::take(&mut utterance),
                                history.clone(),
                                turn_tx.clone(),
                                speech_tx.clone(),
                                cancel.clone(),
                                timings,
                            );
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

                        if past_grace && barge_ms >= BARGE_IN_MS {
                            tracing::info!("barge-in");
                            cancel.cancel();
                            llm_running = false;
                            log_turn(&mut turn, &tts, hold_base_ms, true, stt_backend, &llm_model);
                            tts.interrupt()?;
                            vad.reset();
                            // Seed the new turn from the pre-roll so the words
                            // that did the interrupting are part of it.
                            utterance = tail(&pre_roll, PRE_ROLL);
                            heard_speech = true;
                            silence_ms = 0;
                            utterance_ms = 0;
                            barge_ms = 0;
                            state = State::Listening;
                        } else if !llm_running && tts.idle() {
                            tracing::info!("idle");
                            log_turn(&mut turn, &tts, hold_base_ms, false, stt_backend, &llm_model);
                            vad.reset();
                            state = State::Idle;
                        }
                    }
                }
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
                    Turn::Heard(text) => tracing::info!(user = %text, "heard"),
                    Turn::Done { user, reply } => {
                        llm_running = false;
                        if !reply.is_empty() {
                            tracing::info!(ira = %reply, "reply");
                            history.push((user, reply));
                            // ponytail: fixed-window history. kortex-memory
                            // replaces this with real summarisation and recall.
                            if history.len() > 8 {
                                history.remove(0);
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
                                let _ = tts.say(SAY_STT_FAILED);
                            }
                            Fail::Llm => {
                                tracing::error!("llm failed");
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
fn log_turn(
    turn: &mut Option<metrics::Turn>,
    tts: &tts::Tts,
    hold_base_ms: u64,
    barged: bool,
    backend: &str,
    model: &str,
) {
    if let Some(t) = turn.take() {
        let first_audio = tts
            .first_audio_ms()
            .map(|a| a.saturating_sub(hold_base_ms));
        t.log(first_audio, barged, backend, model);
    }
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

        match llm::stream(&client, &history, &text, speech, cancel, &timings).await {
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
