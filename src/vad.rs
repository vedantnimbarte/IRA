//! Silero VAD v5. Drives both endpointing (when did the user stop) and
//! barge-in (did the user start while IRA was talking).
//!
//! The model takes 576 samples per step, not 512: the 64 samples before the
//! chunk are prepended as context, exactly as silero-vad's own Python wrapper
//! does. Feed it a bare 512 and it runs without complaint -- the input shape is
//! dynamic -- and reports no speech, ever. Nothing errors, endpointing simply
//! never fires and no turn completes.

use anyhow::Result;
use ort::session::Session;
use ort::value::Tensor;
use std::path::Path;

/// Silero v5 steps 512 samples @ 16 kHz (32 ms). This is the caller's unit.
pub const CHUNK: usize = 512;
pub const CHUNK_MS: u64 = 32;
/// Samples of the previous chunk the model wants ahead of this one.
const CONTEXT: usize = 64;

pub struct Vad {
    session: Session,
    state: Vec<f32>,
    /// Tail of the previous chunk. Zeroed at the start of a turn.
    context: Vec<f32>,
    pending: Vec<f32>,
    threshold: f32,
}

impl Vad {
    pub fn new(model: &Path, threshold: f32) -> Result<Self> {
        Ok(Self {
            session: Session::builder()?.commit_from_file(model)?,
            state: vec![0.0; 2 * 128],
            context: vec![0.0; CONTEXT],
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
        let mut input = Vec::with_capacity(CONTEXT + CHUNK);
        input.extend_from_slice(&self.context);
        input.extend_from_slice(chunk);

        let outs = self.session.run(ort::inputs![
            Tensor::from_array(([1_usize, CONTEXT + CHUNK], input))?,
            Tensor::from_array(([2_usize, 1, 128], self.state.clone()))?,
            // The model declares `sr` as a scalar, so pass rank 0.
            Tensor::from_array((vec![] as Vec<usize>, vec![16_000_i64]))?,
        ])?;
        let (_, prob) = outs[0].try_extract_tensor::<f32>()?;
        let speech = prob[0] > self.threshold;
        // Output 1 is the recurrent state; it must be carried forward or the
        // model resets its notion of context every chunk.
        let (_, next) = outs[1].try_extract_tensor::<f32>()?;
        self.state = next.to_vec();
        self.context = chunk[CHUNK - CONTEXT..].to_vec();
        Ok(speech)
    }

    /// Called on state transitions so a new turn does not inherit old context.
    pub fn reset(&mut self) {
        self.state = vec![0.0; 2 * 128];
        self.context = vec![0.0; CONTEXT];
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

    /// The test that matters, and the one whose absence hid a dead VAD.
    ///
    /// "Silence reads as silence" is satisfied by a VAD that reports no speech
    /// under every condition, which is exactly what feeding the model 512
    /// samples instead of 576 produces. Only real speech distinguishes the two.
    #[test]
    fn speech_reads_as_speech() {
        let Some(m) = models() else {
            eprintln!("skipped: run scripts/fetch-models.ps1");
            return;
        };
        let fixture = std::path::Path::new("corpus/speech-16k.wav");
        let Ok((samples, sr, channels)) = crate::audio::read_wav(fixture) else {
            eprintln!("skipped: {} missing", fixture.display());
            return;
        };
        assert_eq!(sr, 16_000, "fixture must already be at the model's rate");
        assert_eq!(channels, 1);

        let mut vad = Vad::new(&m.join("silero_vad.onnx"), 0.5).unwrap();
        let verdicts = vad.push(&samples).unwrap();
        let speech = verdicts.iter().filter(|&&s| s).count();
        // The clip is continuous speech with a little room at each end, so a
        // healthy VAD calls most of it speech. A broken one calls none of it.
        assert!(
            speech * 2 > verdicts.len(),
            "only {speech} of {} chunks read as speech -- is the 64-sample \
             context still being prepended?",
            verdicts.len()
        );
    }

    /// Context must not leak across turns: a new turn starts from silence.
    #[test]
    fn reset_clears_the_context() {
        let Some(m) = models() else {
            eprintln!("skipped: run scripts/fetch-models.ps1");
            return;
        };
        let mut vad = Vad::new(&m.join("silero_vad.onnx"), 0.5).unwrap();
        vad.push(&vec![0.5; CHUNK]).unwrap();
        assert!(vad.context.iter().any(|&s| s != 0.0));
        vad.reset();
        assert!(vad.context.iter().all(|&s| s == 0.0));
        assert_eq!(vad.context.len(), CONTEXT);
    }
}
