//! Silero VAD v5. Drives both endpointing (when did the user stop) and
//! barge-in (did the user start while IRA was talking).

use anyhow::Result;
use ort::session::Session;
use ort::value::Tensor;
use std::path::Path;

/// Silero v5 requires exactly 512 samples @ 16 kHz (32 ms).
pub const CHUNK: usize = 512;
pub const CHUNK_MS: u64 = 32;

pub struct Vad {
    session: Session,
    state: Vec<f32>,
    pending: Vec<f32>,
    threshold: f32,
}

impl Vad {
    pub fn new(model: &Path, threshold: f32) -> Result<Self> {
        Ok(Self {
            session: Session::builder()?.commit_from_file(model)?,
            state: vec![0.0; 2 * 128],
            pending: Vec::new(),
            threshold,
        })
    }

    /// Returns one speech/not-speech verdict per 512-sample chunk consumed.
    pub fn push(&mut self, samples: &[f32]) -> Result<Vec<bool>> {
        self.pending.extend_from_slice(samples);
        let mut out = Vec::new();
        while self.pending.len() >= CHUNK {
            let chunk: Vec<f32> = self.pending.drain(..CHUNK).collect();
            out.push(self.step(&chunk)?);
        }
        Ok(out)
    }

    fn step(&mut self, chunk: &[f32]) -> Result<bool> {
        let outs = self.session.run(ort::inputs![
            Tensor::from_array(([1_usize, CHUNK], chunk.to_vec()))?,
            Tensor::from_array(([2_usize, 1, 128], self.state.clone()))?,
            Tensor::from_array(([1_usize], vec![16_000_i64]))?,
        ])?;
        let (_, prob) = outs[0].try_extract_tensor::<f32>()?;
        let speech = prob[0] > self.threshold;
        // Output 1 is the recurrent state; it must be carried forward or the
        // model resets its notion of context every chunk.
        let (_, next) = outs[1].try_extract_tensor::<f32>()?;
        self.state = next.to_vec();
        Ok(speech)
    }

    /// Called on state transitions so a new turn does not inherit old context.
    pub fn reset(&mut self) {
        self.state = vec![0.0; 2 * 128];
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Models are a download step, so skip rather than fail on a fresh clone.
    fn models() -> Option<std::path::PathBuf> {
        let p = std::path::PathBuf::from("models");
        p.join("silero_vad.onnx").exists().then_some(p)
    }

    #[test]
    fn one_verdict_per_chunk_and_silence_reads_as_silence() {
        let Some(m) = models() else {
            eprintln!("skipped: run scripts/fetch-models.ps1");
            return;
        };
        let mut vad = Vad::new(&m.join("silero_vad.onnx"), 0.5).unwrap();
        // Three full chunks plus a remainder that must stay buffered.
        let verdicts = vad.push(&vec![0.0; CHUNK * 3 + 100]).unwrap();
        assert_eq!(verdicts.len(), 3);
        assert!(verdicts.iter().all(|&s| !s), "silence classified as speech");
        assert_eq!(vad.pending.len(), 100);
    }
}
