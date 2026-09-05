//! openWakeWord: melspectrogram -> embedding -> classifier.
//!
//! Apache-2.0 models, free for commercial use. Ships a pretrained `hey_jarvis`
//! model, which is what the prototype uses. Training a real "IRA" wake word is a
//! Colab run against the same three-stage chain -- only the last .onnx changes.

use anyhow::Result;
use ort::session::Session;
use ort::value::Tensor;
use std::collections::VecDeque;
use std::path::Path;

/// openWakeWord's fixed step: 80 ms @ 16 kHz.
pub const CHUNK: usize = 1280;
const MEL_BINS: usize = 32;
const MEL_HOP: usize = 160;
/// Mel frames the embedding model consumes per window.
const EMB_WINDOW: usize = 76;
/// Embeddings the classifier consumes.
const CLS_WINDOW: usize = 16;
/// Extra context so streaming melspec frames match a batch computation.
const MEL_PAD: usize = 480;

pub struct WakeWord {
    mel: Session,
    emb: Session,
    cls: Session,
    /// Raw 16 kHz audio, trimmed to what melspec still needs.
    raw: VecDeque<f32>,
    /// Flattened mel frames (MEL_BINS each).
    mels: VecDeque<f32>,
    /// Flattened embeddings (96 each).
    embs: VecDeque<f32>,
    pending: Vec<f32>,
    threshold: f32,
    /// Chunks to ignore after a trigger, so one utterance fires once.
    refractory: u32,
}

impl WakeWord {
    pub fn new(models: &Path, wakeword: &str, threshold: f32) -> Result<Self> {
        let load = |n: &str| -> Result<Session> {
            Ok(Session::builder()?.commit_from_file(models.join(n))?)
        };
        Ok(Self {
            mel: load("melspectrogram.onnx")?,
            emb: load("embedding_model.onnx")?,
            cls: load(wakeword)?,
            raw: VecDeque::new(),
            mels: VecDeque::new(),
            embs: VecDeque::new(),
            pending: Vec::new(),
            threshold,
            refractory: 0,
        })
    }

    /// Feeds 16 kHz mono audio. Returns the score of any chunk that fired.
    pub fn push(&mut self, samples: &[f32]) -> Result<Option<f32>> {
        self.pending.extend_from_slice(samples);
        let mut hit = None;
        while self.pending.len() >= CHUNK {
            let chunk: Vec<f32> = self.pending.drain(..CHUNK).collect();
            if let Some(score) = self.step(&chunk)? {
                hit = Some(score);
            }
        }
        Ok(hit)
    }

    fn step(&mut self, chunk: &[f32]) -> Result<Option<f32>> {
        self.raw.extend(chunk.iter().copied());
        let need = CHUNK + MEL_PAD;
        while self.raw.len() > need {
            self.raw.pop_front();
        }
        if self.raw.len() < need {
            return Ok(None);
        }

        // 1. Melspectrogram over the padded window; keep only the new frames.
        let window: Vec<f32> = self.raw.iter().copied().collect();
        let n = window.len();
        let out = self.mel.run(ort::inputs![Tensor::from_array(([1_usize, n], window))?])?;
        let (_, mel) = out[0].try_extract_tensor::<f32>()?;
        let new_frames = CHUNK / MEL_HOP;
        let tail = new_frames * MEL_BINS;
        if mel.len() < tail {
            return Ok(None);
        }
        // openWakeWord's normalization, applied outside the graph.
        self.mels.extend(mel[mel.len() - tail..].iter().map(|v| v / 10.0 + 2.0));
        while self.mels.len() > EMB_WINDOW * MEL_BINS {
            self.mels.pop_front();
        }
        if self.mels.len() < EMB_WINDOW * MEL_BINS {
            return Ok(None);
        }

        // 2. One embedding per chunk, from the trailing 76 mel frames.
        let mels: Vec<f32> = self.mels.iter().copied().collect();
        let out = self.emb.run(ort::inputs![Tensor::from_array((
            [1_usize, EMB_WINDOW, MEL_BINS, 1],
            mels
        ))?])?;
        let (_, emb) = out[0].try_extract_tensor::<f32>()?;
        let dim = emb.len();
        self.embs.extend(emb.iter().copied());
        while self.embs.len() > CLS_WINDOW * dim {
            self.embs.pop_front();
        }
        if self.embs.len() < CLS_WINDOW * dim {
            return Ok(None);
        }

        // 3. Classify the trailing 16 embeddings.
        if self.refractory > 0 {
            self.refractory -= 1;
            return Ok(None);
        }
        let embs: Vec<f32> = self.embs.iter().copied().collect();
        let out = self
            .cls
            .run(ort::inputs![Tensor::from_array(([1_usize, CLS_WINDOW, dim], embs))?])?;
        let (_, score) = out[0].try_extract_tensor::<f32>()?;
        let score = score[0];
        if score > self.threshold {
            // ~1.3 s of silence before we will listen for the word again.
            self.refractory = 16;
            self.embs.clear();
            return Ok(Some(score));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Option<std::path::PathBuf> {
        let p = std::path::PathBuf::from("models");
        p.join("melspectrogram.onnx").exists().then_some(p)
    }

    /// Exercises all three model stages with real weights. This is the only
    /// check on the tensor shapes threaded between melspec, embedding and
    /// classifier -- get one wrong and ort panics here rather than at 3am.
    #[test]
    fn three_stage_chain_runs_and_does_not_fire_on_a_tone() {
        let Some(m) = models() else {
            eprintln!("skipped: run scripts/fetch-models.ps1");
            return;
        };
        let mut w = WakeWord::new(&m, "hey_jarvis_v0.1.onnx", 0.5).unwrap();
        // 3 s of quiet 440 Hz. The chain needs ~2.2 s before the classifier
        // can run at all: 76 mel frames to reach the first embedding, then 16
        // embeddings at 80 ms each. Anything shorter silently tests two stages.
        let audio: Vec<f32> = (0..48_000)
            .map(|i| (i as f32 / 16_000.0 * 440.0 * std::f32::consts::TAU).sin() * 0.05)
            .collect();
        assert_eq!(w.push(&audio).unwrap(), None, "a sine wave is not a wake word");
        assert_eq!(w.embs.len(), CLS_WINDOW * 96, "embedding window not filled");
    }
}
