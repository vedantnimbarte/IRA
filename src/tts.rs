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
use std::sync::atomic::{AtomicU64, Ordering};
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

pub struct Tts {
    _stream: MixerDeviceSink,
    sink: Arc<Player>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    /// Millis since process start when piper last produced audio bytes.
    last_audio: Arc<AtomicU64>,
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
                            .chunks_exact(2)
                            .map(|p| i16::from_le_bytes([p[0], p[1]]) as f32 / 32768.0)
                            .collect();
                        if !samples.is_empty() {
                            last.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
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
        Ok(())
    }

    /// Barge-in: silence immediately, then discard piper's in-flight work.
    pub fn interrupt(&mut self) -> Result<()> {
        self.sink.clear();
        // clear() also pauses the sink in rodio; re-arm it for the next reply.
        self.sink.play();
        self.stdin.take();
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // ponytail: respawn costs ~200-300 ms, paid only when interrupted. If
        // that lands badly in testing, keep a warm spare process instead.
        self.spawn()
    }

    /// True once the queue has drained and piper has been quiet a beat.
    pub fn idle(&self) -> bool {
        // saturating: the reader thread can store a timestamp between these two
        // reads, and a wrapped u64 would report "quiet" during active speech.
        let quiet = (self.started.elapsed().as_millis() as u64)
            .saturating_sub(self.last_audio.load(Ordering::Relaxed));
        self.sink.empty() && quiet > DRAIN_QUIET.as_millis() as u64
    }

    /// Short rising blip so you know the wake word landed before IRA speaks.
    pub fn chirp(&self) {
        const SR: u32 = 24_000;
        let mut s = Vec::with_capacity(SR as usize / 10);
        for freq in [660.0f32, 880.0] {
            let n = SR as usize / 25; // 40 ms per tone
            for k in 0..n {
                let t = k as f32 / SR as f32;
                // Fade each tone in and out so it clicks rather than pops.
                let env = (k as f32 / n as f32 * std::f32::consts::PI).sin();
                s.push((t * freq * std::f32::consts::TAU).sin() * env * 0.18);
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
