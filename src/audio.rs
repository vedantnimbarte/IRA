//! Mic capture -> 16 kHz mono f32 frames.
//!
//! Input only. Playback lives in `tts.rs` (rodio owns the output device).
//!
//! The loop does not care where frames come from, so `IRA_AUDIO_FILE` replays a
//! WAV through this same path instead of opening the microphone. That is what
//! makes wake detection, endpointing and barge-in testable at all: everything
//! downstream of here is deterministic once the frames are.

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::path::Path;
use tokio::sync::mpsc::{channel, Receiver};

pub const SR: u32 = 16_000;

/// Cheap fixed-ratio resampler: one-pole lowpass, then linear interpolation.
///
/// ponytail: aliasing is audible-ish on music, inaudible on speech, and whisper
/// does not care. Swap in `rubato` if STT word-error-rate ever measures worse
/// than a reference 48k->16k sox resample.
struct Resampler {
    ratio: f64,
    pos: f64,
    last: f32,
    lp: f32,
    alpha: f32,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        // Cutoff just under the output Nyquist so decimation does not fold.
        let alpha = if from > to {
            (to as f32) / (from as f32)
        } else {
            1.0
        };
        Self { ratio: from as f64 / to as f64, pos: 0.0, last: 0.0, lp: 0.0, alpha }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        for &x in input {
            self.lp += self.alpha * (x - self.lp);
            // Emit every output sample that falls inside this input interval.
            while self.pos < 1.0 {
                let t = self.pos as f32;
                out.push(self.last + (self.lp - self.last) * t);
                self.pos += self.ratio;
            }
            self.pos -= 1.0;
            self.last = self.lp;
        }
    }
}

/// One openWakeWord step: 80 ms at 16 kHz. Replay uses it as the frame size so
/// a file behaves like a real capture callback rather than one giant buffer.
const FRAME: usize = 1280;

/// Silence appended after a replayed file, overridable with `IRA_TAIL_MS`.
///
/// Endpointing needs silence *after* speech to close the turn, and the reply
/// needs frames to arrive while it plays -- the state machine only advances on a
/// frame. Without this a corpus file would have to carry its own trailing
/// silence, and every one that forgot would hang instead of answering.
///
/// The default is enough to close the turn. A benchmark that wants the whole
/// reply spoken needs longer, because the model's round trip happens inside
/// this window.
const TAIL_SILENCE_MS: usize = 3_000;

fn tail_silence_ms() -> usize {
    std::env::var("IRA_TAIL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(TAIL_SILENCE_MS)
}

pub struct Mic {
    /// `None` when replaying a file.
    _stream: Option<cpal::Stream>,
    pub rx: Receiver<Vec<f32>>,
}

impl Mic {
    /// The microphone, or a WAV replay when `IRA_AUDIO_FILE` is set.
    pub fn open() -> Result<Self> {
        match std::env::var("IRA_AUDIO_FILE") {
            Ok(path) => Self::from_file(Path::new(&path)),
            Err(_) => Self::from_device(),
        }
    }

    /// Replays a WAV as if it were arriving from the microphone.
    ///
    /// Frames are paced in real time so endpointing and barge-in timing behave
    /// as they would live. `IRA_CLOCK=virtual` drops the pacing for CI, where
    /// waiting out a ten-second utterance twenty times is the whole run.
    fn from_file(path: &Path) -> Result<Self> {
        let (samples, in_sr, channels) = read_wav(path)?;
        let mono: Vec<f32> = samples
            .chunks(channels)
            .map(|f| f.iter().sum::<f32>() / channels as f32)
            .collect();

        let mut rs = Resampler::new(in_sr, SR);
        let mut out = Vec::new();
        rs.process(&mono, &mut out);

        tracing::info!(
            file = %path.display(),
            in_sr,
            channels,
            secs = out.len() as f32 / SR as f32,
            "replaying audio file"
        );

        let realtime = std::env::var("IRA_CLOCK").as_deref() != Ok("virtual");
        let (tx, rx) = channel::<Vec<f32>>(64);
        out.extend(std::iter::repeat_n(
            0.0,
            tail_silence_ms() * SR as usize / 1000,
        ));
        std::thread::spawn(move || {
            for frame in out.chunks(FRAME) {
                if realtime {
                    std::thread::sleep(std::time::Duration::from_millis(
                        (frame.len() as u64 * 1000) / SR as u64,
                    ));
                }
                if tx.blocking_send(frame.to_vec()).is_err() {
                    return;
                }
            }
            // Dropping tx closes the channel, which the main loop reads as
            // end of input and exits -- that is what makes a replay a test.
        });

        Ok(Self { _stream: None, rx })
    }

    /// Opens the default input device and downmixes/resamples to 16 kHz mono.
    fn from_device() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?;
        let config = device.default_input_config()?;
        let in_sr = config.sample_rate();
        let channels = config.channels() as usize;

        tracing::info!(
            device = device.description().map(|d| d.name().to_string()).unwrap_or_default(),
            in_sr,
            channels,
            "mic open"
        );

        // Bounded: if the consumer stalls we drop frames rather than grow forever.
        // Dropping audio is the correct failure mode for a live mic.
        let (tx, rx) = channel::<Vec<f32>>(64);
        let mut rs = Resampler::new(in_sr, SR);
        let mut mono = Vec::new();
        let mut out = Vec::new();

        let stream = device.build_input_stream(
            config.into(),
            move |data: &[f32], _| {
                mono.clear();
                mono.extend(data.chunks(channels).map(|f| {
                    f.iter().sum::<f32>() / channels as f32
                }));
                out.clear();
                rs.process(&mono, &mut out);
                if !out.is_empty() {
                    // try_send: never block the audio callback.
                    let _ = tx.try_send(out.clone());
                }
            },
            |e| tracing::error!(?e, "mic stream error"),
            None,
        )?;
        stream.play()?;

        Ok(Self { _stream: Some(stream), rx })
    }
}

/// Minimal 16-bit PCM WAV reader, returning interleaved samples, sample rate
/// and channel count.
///
/// Chunks are walked rather than read at fixed offsets: piper writes a bare
/// RIFF/fmt/data file, but anything that has been through sox or ffmpeg carries
/// LIST or fact chunks before the audio, and assuming offset 44 reads those as
/// samples.
pub(crate) fn read_wav(path: &Path) -> Result<(Vec<f32>, u32, usize)> {
    let b = std::fs::read(path)?;
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err(anyhow!("{} is not a RIFF/WAVE file", path.display()));
    }

    let mut pos = 12;
    let mut fmt: Option<(usize, u32, u16)> = None;
    let mut data: Option<(usize, usize)> = None;

    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let size = u32::from_le_bytes(b[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        match id {
            b"fmt " if size >= 16 && body + 16 <= b.len() => {
                fmt = Some((
                    u16::from_le_bytes(b[body + 2..body + 4].try_into().unwrap()) as usize,
                    u32::from_le_bytes(b[body + 4..body + 8].try_into().unwrap()),
                    u16::from_le_bytes(b[body + 14..body + 16].try_into().unwrap()),
                ));
            }
            b"data" => data = Some((body, (body + size).min(b.len()))),
            _ => {}
        }
        // Chunk bodies are word-aligned; an odd size carries a pad byte.
        pos = body + size + (size & 1);
    }

    let (channels, rate, bits) = fmt.ok_or_else(|| anyhow!("no fmt chunk"))?;
    let (start, end) = data.ok_or_else(|| anyhow!("no data chunk"))?;
    if bits != 16 {
        return Err(anyhow!("only 16-bit PCM is supported, got {bits}-bit"));
    }
    if channels == 0 {
        return Err(anyhow!("fmt chunk claims zero channels"));
    }

    let samples = b[start..end]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| i16::from_le_bytes(*p) as f32 / 32768.0)
        .collect();
    Ok((samples, rate, channels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_48k_to_16k_thirds_the_samples() {
        let mut rs = Resampler::new(48_000, 16_000);
        let mut out = Vec::new();
        rs.process(&vec![0.5f32; 4800], &mut out);
        // 1/3 of input, within one sample of rounding.
        assert!((out.len() as i64 - 1600).abs() <= 1, "got {}", out.len());
    }

    /// Builds a WAV with a junk chunk before the audio, which is what a file
    /// that has been through sox or ffmpeg looks like.
    fn wav_with_junk_chunk(samples: &[i16], rate: u32, channels: u16) -> Vec<u8> {
        let mut fmt = Vec::new();
        fmt.extend(&1u16.to_le_bytes()); // PCM
        fmt.extend(&channels.to_le_bytes());
        fmt.extend(&rate.to_le_bytes());
        fmt.extend(&(rate * channels as u32 * 2).to_le_bytes());
        fmt.extend(&(channels * 2).to_le_bytes());
        fmt.extend(&16u16.to_le_bytes()); // bits

        let mut data = Vec::new();
        for s in samples {
            data.extend(&s.to_le_bytes());
        }

        let mut body = Vec::new();
        body.extend(b"WAVE");
        body.extend(b"fmt ");
        body.extend(&(fmt.len() as u32).to_le_bytes());
        body.extend(&fmt);
        // Odd-sized chunk: exercises the word-alignment pad byte too.
        body.extend(b"LIST");
        body.extend(&3u32.to_le_bytes());
        body.extend(b"abc");
        body.push(0); // word-alignment pad for the odd-sized chunk
        body.extend(b"data");
        body.extend(&(data.len() as u32).to_le_bytes());
        body.extend(&data);

        let mut out = Vec::new();
        out.extend(b"RIFF");
        out.extend(&(body.len() as u32).to_le_bytes());
        out.extend(&body);
        out
    }

    /// Reading at a fixed offset 44 would swallow the LIST chunk as audio, and
    /// the replay would start with a burst of noise instead of the recording.
    #[test]
    fn wav_reader_walks_chunks_and_keeps_the_samples() {
        let pcm: Vec<i16> = vec![0, 16_384, -16_384, 32_767, -32_768, 100];
        let bytes = wav_with_junk_chunk(&pcm, 8_000, 2);
        let dir = std::env::temp_dir().join("ira-wav-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chunks.wav");
        std::fs::write(&path, &bytes).unwrap();

        let (samples, rate, channels) = read_wav(&path).unwrap();
        assert_eq!(rate, 8_000);
        assert_eq!(channels, 2);
        assert_eq!(samples.len(), pcm.len());
        assert!((samples[0] - 0.0).abs() < 1e-6);
        assert!((samples[1] - 0.5).abs() < 1e-4, "got {}", samples[1]);
        // Full negative scale must stay in range, not wrap.
        assert!((samples[4] + 1.0).abs() < 1e-6, "got {}", samples[4]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wav_reader_rejects_a_non_wav() {
        let dir = std::env::temp_dir().join("ira-wav-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not.wav");
        std::fs::write(&path, b"this is not a wav file at all").unwrap();
        assert!(read_wav(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resample_passthrough_preserves_length() {
        let mut rs = Resampler::new(16_000, 16_000);
        let mut out = Vec::new();
        rs.process(&vec![0.25f32; 1000], &mut out);
        assert!((out.len() as i64 - 1000).abs() <= 1, "got {}", out.len());
    }
}
