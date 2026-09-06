//! Piper TTS as a long-lived subprocess, playing through rodio.
//!
//! One piper process stays alive and takes sentences on stdin, so there is no
//! model-load cost per reply. Barge-in clears the rodio queue (instant silence)
//! and respawns piper to throw away whatever it was mid-way through generating.

use anyhow::{anyhow, Context, Result};
use rodio::buffer::SamplesBuffer;
use rodio::{ChannelCount, DeviceSinkBuilder, MixerDeviceSink, Player, SampleRate};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::num::NonZero;
use std::time::{Duration, Instant};

/// rodio 0.22 types these as NonZero, so build them once.
fn mono() -> ChannelCount {
    NonZero::new(1).expect("1 is nonzero")
}

/// Silence this long, with the queue drained, means the reply is finished.
/// Piper gives no end-of-utterance marker, so quiet is the only signal.
const DRAIN_QUIET: Duration = Duration::from_millis(400);

/// How long piper gets to produce the first sample of a sentence it was asked
/// for, before we conclude it never will.
///
/// Synthesis runs at roughly a tenth of real time, so a long sentence can take
/// well over a second to start. Judging that by `DRAIN_QUIET` abandons the reply
/// before it has said a word -- and the longer the answer, the more certain the
/// failure, which is the wrong way round.
const SYNTH_GRACE: Duration = Duration::from_secs(10);

pub struct Tts {
    _stream: MixerDeviceSink,
    sink: Arc<Player>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    /// Millis since process start when piper last produced audio bytes.
    last_audio: Arc<AtomicU64>,
    /// Piper has been given a sentence it has not started rendering yet.
    awaiting: Arc<AtomicBool>,
    /// Millis since process start when audio first reached the speaker for the
    /// current turn, or 0 for "nothing yet". The barge-in grace window is
    /// measured from here rather than from the start of the turn: STT and the
    /// model can spend the whole window before IRA has said a word, which left
    /// the grace protecting nothing.
    first_audio: Arc<AtomicU64>,
    started: Instant,
    piper: PathBuf,
    voice: PathBuf,
    sample_rate: SampleRate,
}

impl Tts {
    pub fn new(piper: &Path, voice: &Path) -> Result<Self> {
        // Piper voices ship a sidecar JSON; the sample rate lives there and
        // varies by voice. Guessing it makes everything play at the wrong pitch.
        let cfg = voice.with_extension("onnx.json");
        let sample_rate = std::fs::read_to_string(&cfg)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v["audio"]["sample_rate"].as_u64())
            .and_then(|r| NonZero::new(r as u32))
            .unwrap_or(NonZero::new(22_050).expect("nonzero"));

        let stream = DeviceSinkBuilder::open_default_sink()
            .context("no default output device")?;
        let sink = Arc::new(Player::connect_new(stream.mixer()));

        let mut tts = Self {
            _stream: stream,
            sink,
            child: None,
            stdin: None,
            last_audio: Arc::new(AtomicU64::new(0)),
            awaiting: Arc::new(AtomicBool::new(false)),
            first_audio: Arc::new(AtomicU64::new(0)),
            started: Instant::now(),
            piper: piper.to_path_buf(),
            voice: voice.to_path_buf(),
            sample_rate,
        };
        tts.spawn()?;
        Ok(tts)
    }

    fn spawn(&mut self) -> Result<()> {
        let mut child = Command::new(&self.piper)
            .arg("--model")
            .arg(&self.voice)
            .arg("--output_raw")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited so piper's own errors are visible instead of swallowed.
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawn piper at {}", self.piper.display()))?;

        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no piper stdout"))?;
        self.stdin = child.stdin.take();
        self.child = Some(child);

        let sink = self.sink.clone();
        let last = self.last_audio.clone();
        let awaiting = self.awaiting.clone();
        let first = self.first_audio.clone();
        let started = self.started;
        let sr = self.sample_rate;

        // Blocking reader: piper writes raw mono i16 LE continuously.
        std::thread::spawn(move || {
            let mut rdr = stdout;
            let mut buf = [0u8; 4096];
            let mut carry: Option<u8> = None;
            loop {
                match rdr.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut bytes: Vec<u8> = Vec::with_capacity(n + 1);
                        if let Some(c) = carry.take() {
                            bytes.push(c);
                        }
                        bytes.extend_from_slice(&buf[..n]);
                        // A read can split a sample across chunk boundaries.
                        if bytes.len() % 2 == 1 {
                            carry = bytes.pop();
                        }
                        let samples: Vec<f32> = bytes
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|p| i16::from_le_bytes(*p) as f32 / 32768.0)
                            .collect();
                        if !samples.is_empty() {
                            let now = started.elapsed().as_millis() as u64;
                            last.store(now, Ordering::Relaxed);
                            awaiting.store(false, Ordering::Relaxed);
                            // Only the first sample of a turn wins; 0 means the
                            // turn has produced no sound yet.
                            let _ = first.compare_exchange(
                                0,
                                now.max(1),
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            );
                            sink.append(SamplesBuffer::new(mono(), sr, samples));
                        }
                    }
                }
            }
        });
        Ok(())
    }

    /// Queues one sentence. Piper starts generating as soon as the line lands.
    pub fn say(&mut self, text: &str) -> Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(|| anyhow!("piper stdin closed"))?;
        writeln!(stdin, "{}", text.replace('\n', " "))?;
        stdin.flush()?;
        self.last_audio
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
        self.awaiting.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Millis since this `Tts` was created. The clock the other timings share.
    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Millis since start-up when this turn first made a sound, if it has.
    pub fn first_audio_ms(&self) -> Option<u64> {
        match self.first_audio.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(ms),
        }
    }

    /// Called when IRA takes the floor, so `first_audio_ms` measures this turn.
    pub fn begin_turn(&self) {
        self.first_audio.store(0, Ordering::Relaxed);
        self.sink.set_volume(1.0);
    }

    /// Drop to a background level at the first hint of speech, before it has
    /// been confirmed as an interruption.
    ///
    /// A hard cut 250 ms later is correct but sounds like a machine being
    /// switched off. Ducking first makes the same moment sound like yielding,
    /// and if the speech turns out to be a cough the volume comes back and
    /// nothing was lost.
    pub fn duck(&self) {
        self.sink.set_volume(0.35);
    }

    pub fn unduck(&self) {
        self.sink.set_volume(1.0);
    }

    /// Barge-in: silence immediately, then discard piper's in-flight work.
    pub fn interrupt(&mut self) -> Result<()> {
        self.sink.clear();
        // clear() also pauses the sink in rodio; re-arm it for the next reply.
        self.sink.play();
        // A duck must never outlive the turn that caused it, or the next reply
        // is quiet for no reason anyone could explain.
        self.sink.set_volume(1.0);
        self.stdin.take();
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // Whatever it had been asked for is gone with it.
        self.awaiting.store(false, Ordering::Relaxed);
        // ponytail: respawn costs ~200-300 ms, paid only when interrupted. If
        // that lands badly in testing, keep a warm spare process instead.
        self.spawn()
    }

    /// True once the queue has drained and piper has been quiet a beat.
    pub fn idle(&self) -> bool {
        if !self.sink.empty() {
            return false;
        }
        // saturating: the reader thread can store a timestamp between these two
        // reads, and a wrapped u64 would report "quiet" during active speech.
        let quiet = (self.started.elapsed().as_millis() as u64)
            .saturating_sub(self.last_audio.load(Ordering::Relaxed));

        if self.awaiting.load(Ordering::Relaxed) {
            // Asked for something not yet delivered. An empty queue here means
            // piper is still thinking, not that the reply is over -- but a dead
            // piper must not strand the loop holding the floor either.
            return quiet > SYNTH_GRACE.as_millis() as u64;
        }
        quiet > DRAIN_QUIET.as_millis() as u64
    }

    /// Short rising blip so you know the wake word landed before IRA speaks.
    pub fn chirp(&self) {
        self.tone(&[660.0, 880.0], 0.18);
    }

    /// A single soft pip: something you asked for a while ago has finished.
    ///
    /// Deliberately not speech. It fires the moment the job lands, which may be
    /// while you are mid-sentence with someone else; the words wait until IRA
    /// has the floor legitimately.
    pub fn pip(&self) {
        self.tone(&[880.0], 0.12);
    }

    /// Falling two-tone for a failure IRA could not say out loud.
    ///
    /// This exists because TTS cannot announce its own death: if piper is gone,
    /// every spoken error message is also gone, and the user gets the silence
    /// that this whole change is meant to remove. Falling and quieter than the
    /// wake chirp, so the two are never confused with your back to the machine.
    pub fn error_tone(&self) {
        self.tone(&[440.0, 330.0], 0.14);
    }

    /// A sequence of 40 ms tones, each faded in and out so it clicks rather
    /// than pops.
    fn tone(&self, freqs: &[f32], gain: f32) {
        const SR: u32 = 24_000;
        let n = SR as usize / 25; // 40 ms
        let mut s = Vec::with_capacity(n * freqs.len());
        for &freq in freqs {
            for k in 0..n {
                let t = k as f32 / SR as f32;
                let env = (k as f32 / n as f32 * std::f32::consts::PI).sin();
                s.push((t * freq * std::f32::consts::TAU).sin() * env * gain);
            }
        }
        let sr = NonZero::new(SR).expect("nonzero");
        self.sink.append(SamplesBuffer::new(mono(), sr, s));
    }
}

impl Drop for Tts {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
        }
    }
}
