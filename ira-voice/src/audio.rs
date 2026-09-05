//! Mic capture -> 16 kHz mono f32 frames.
//!
//! Input only. Playback lives in `tts.rs` (rodio owns the output device).

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
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

pub struct Mic {
    _stream: cpal::Stream,
    pub rx: Receiver<Vec<f32>>,
}

impl Mic {
    /// Opens the default input device and downmixes/resamples to 16 kHz mono.
    pub fn open() -> Result<Self> {
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

        Ok(Self { _stream: stream, rx })
    }
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

    #[test]
    fn resample_passthrough_preserves_length() {
        let mut rs = Resampler::new(16_000, 16_000);
        let mut out = Vec::new();
        rs.process(&vec![0.25f32; 1000], &mut out);
        assert!((out.len() as i64 - 1000).abs() <= 1, "got {}", out.len());
    }
}
