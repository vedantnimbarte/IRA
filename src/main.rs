//! IRA duplex voice loop prototype.
//!
//! Idle --wake word--> Listening --silence--> Holding --barge-in--> Listening
//!
//! The only claim this prototype makes is that the loop *feels* right: fast
//! enough to talk to, interruptible mid-sentence, and it does not cut you off
//! when you pause to think. Tools, memory, routing and the UI come after.
//!
//! ponytail: no acoustic echo cancellation. Wear headphones. Without AEC the mic
//! hears IRA's own speech and barge-in fires on her own voice, so speaker mode
//! needs `webrtc-audio-processing` wired into audio.rs before it is usable.

mod audio;
mod llm;
mod stt;
mod tts;
mod vad;
mod wake;

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::path::PathBuf;
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
/// Ignore barge-in briefly after IRA takes the floor, so the tail of the user's
/// own question does not immediately interrupt the answer to it.
const BARGE_IN_GRACE_MS: u64 = 300;

const PRE_ROLL: usize = audio::SR as usize; // 1 s
/// Audio kept from before the wake word fired, so a fast "IRA, what time is it"
/// does not lose the "what".
const PRE_ROLL_KEEP: usize = audio::SR as usize * 2 / 5; // 400 ms

#[derive(Debug, PartialEq)]
enum State {
    Idle,
    Listening,
    /// Thinking and speaking are one state: IRA has the floor, barge-in armed.
    Holding,
}

enum Turn {
    Heard(String),
    Done { user: String, reply: String },
    Failed(String),
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

    let mut wake = wake::WakeWord::new(&models, &wakeword, 0.5)
        .with_context(|| format!("load wake models from {}", models.display()))?;
    let mut vad = vad::Vad::new(&models.join("silero_vad.onnx"), 0.5).context("load silero vad")?;
    let mut tts = tts::Tts::new(&piper, &voice).context("start piper")?;
    let mut mic = audio::Mic::open().context("open microphone")?;

    let client = reqwest::Client::new();
    let (turn_tx, mut turn_rx) = mpsc::channel::<Turn>(16);
    // Sentences flow straight from the LLM stream to the main loop, which owns
    // the TTS handle. Speaking starts before the model finishes writing.
    let (speech_tx, mut speech_rx) = mpsc::channel::<String>(32);

    let mut state = State::Idle;
    let mut history: Vec<(String, String)> = Vec::new();
    let mut cancel = CancellationToken::new();

    let mut pre_roll: VecDeque<f32> = VecDeque::with_capacity(PRE_ROLL);
    let mut utterance: Vec<f32> = Vec::new();
    let mut heard_speech = false;
    let mut silence_ms = 0u64;
    let mut utterance_ms = 0u64;
    let mut barge_ms = 0u64;
    let mut hold_ms = 0u64;
    let mut llm_running = false;

    tracing::info!("ready -- say the wake word; ctrl-c to quit");

    loop {
        tokio::select! {
            Some(frame) = mic.rx.recv() => {
                let frame_ms = (frame.len() as u64 * 1000) / audio::SR as u64;

                pre_roll.extend(frame.iter().copied());
                while pre_roll.len() > PRE_ROLL {
                    pre_roll.pop_front();
                }

                match state {
                    State::Idle => {
                        if let Some(score) = wake.push(&frame)? {
                            tracing::info!(score, "wake");
                            tts.chirp();
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
                            hold_ms = 0;
                            barge_ms = 0;
                            vad.reset();
                            state = State::Holding;
                            spawn_turn(
                                client.clone(),
                                std::mem::take(&mut utterance),
                                history.clone(),
                                turn_tx.clone(),
                                speech_tx.clone(),
                                cancel.clone(),
                            );
                        }
                    }

                    State::Holding => {
                        hold_ms += frame_ms;
                        for speech in vad.push(&frame)? {
                            if speech {
                                barge_ms += vad::CHUNK_MS;
                            } else {
                                barge_ms = 0;
                            }
                        }

                        if hold_ms > BARGE_IN_GRACE_MS && barge_ms >= BARGE_IN_MS {
                            tracing::info!("barge-in");
                            cancel.cancel();
                            llm_running = false;
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
                            vad.reset();
                            state = State::Idle;
                        }
                    }
                }
            }

            Some(sentence) = speech_rx.recv() => {
                // A sentence can arrive just after a barge-in cancelled its turn.
                if !cancel.is_cancelled() && state == State::Holding {
                    if let Err(e) = tts.say(&sentence) {
                        tracing::error!(?e, "tts");
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
                    Turn::Failed(e) => {
                        llm_running = false;
                        tracing::error!("{e}");
                    }
                }
            }

            _ = tokio::signal::ctrl_c() => break,
        }
    }

    Ok(())
}

/// Last `n` samples of the ring buffer, or all of it if it holds fewer.
fn tail(buf: &VecDeque<f32>, n: usize) -> Vec<f32> {
    buf.iter().skip(buf.len().saturating_sub(n)).copied().collect()
}

fn spawn_turn(
    client: reqwest::Client,
    audio: Vec<f32>,
    history: Vec<(String, String)>,
    out: mpsc::Sender<Turn>,
    speech: mpsc::Sender<String>,
    cancel: CancellationToken,
) {
    let cancelled = cancel.clone();
    tokio::spawn(async move {
        let text = match stt::transcribe(&client, &audio, audio::SR).await {
            Ok(t) => t,
            Err(e) => {
                let _ = out.send(Turn::Failed(format!("stt: {e}"))).await;
                return;
            }
        };
        if text.is_empty() || cancel.is_cancelled() {
            let _ = out.send(Turn::Done { user: text, reply: String::new() }).await;
            return;
        }
        let _ = out.send(Turn::Heard(text.clone())).await;

        match llm::stream(&client, &history, &text, speech, cancel).await {
            // A cancelled stream still returns its partial text; that half a
            // sentence must not enter history as if IRA had said it.
            Ok(reply) if !cancelled.is_cancelled() => {
                let _ = out.send(Turn::Done { user: text, reply }).await;
            }
            Ok(_) => {
                let _ = out.send(Turn::Done { user: text, reply: String::new() }).await;
            }
            Err(e) => {
                let _ = out.send(Turn::Failed(format!("llm: {e}"))).await;
            }
        }
    });
}
