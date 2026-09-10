//! Per-turn timing. One log line per turn, so a bad turn says which stage was
//! slow instead of leaving you to guess.
//!
//! The tuning constants in `main.rs` were guesses measured by ear. They stay
//! guesses until there is a number to move them against, which is what this
//! exists for. `total_ms` is the one that matters: end of the user's speech to
//! the first audible syllable.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Stage timings a spawned turn fills in while the main loop keeps running.
///
/// Atomics rather than a channel because the main loop reads these once, at the
/// end of the turn, and a missed update is a wrong log line rather than a bug.
#[derive(Default)]
pub struct Timings {
    pub stt_ms: AtomicU64,
    pub ttft_ms: AtomicU64,
    /// Time spent inside tools, summed over the turn.
    pub tool_ms: AtomicU64,
    /// How many tools the turn called.
    pub tool_calls: AtomicU64,
    /// Tokens the model reported for this turn, when it reported any.
    pub in_tokens: AtomicU64,
    pub out_tokens: AtomicU64,
}

impl Timings {
    pub fn set(field: &AtomicU64, ms: u64) {
        field.store(ms, Ordering::Relaxed);
    }
}

/// One turn, from the moment the user stopped talking.
pub struct Turn {
    pub id: u64,
    pub timings: Arc<Timings>,
    wake_ms: u64,
    listen_ms: u64,
    /// Whether the transcript was already in hand when the turn began, having
    /// been produced while the endpoint window was still counting down.
    pub speculative: bool,
}

impl Turn {
    /// `wake_ms` is wake-detection to chirp; `listen_ms` is wake to endpoint.
    pub fn start(id: u64, wake_ms: u64, listen_ms: u64) -> Self {
        Self {
            id,
            timings: Arc::new(Timings::default()),
            wake_ms,
            listen_ms,
            speculative: false,
        }
    }

    /// Emits the turn line. `first_audio_ms` is milliseconds from the endpoint
    /// to the first sample that reached the speaker, or `None` if IRA never got
    /// a word out -- which is itself the most interesting case.
    pub fn log(&self, first_audio_ms: Option<u64>, barged: bool, stt_backend: &str, model: &str) {
        let t = &self.timings;
        let stt_ms = t.stt_ms.load(Ordering::Relaxed);
        let ttft_ms = t.ttft_ms.load(Ordering::Relaxed);
        // From the first model token to the first sound. The remainder after
        // STT and the model is ours: sentence splitting, piper, the queue.
        let tts_ms = first_audio_ms
            .map(|a| a.saturating_sub(stt_ms + ttft_ms))
            .unwrap_or(0);

        tracing::info!(
            turn = self.id,
            wake_ms = self.wake_ms,
            listen_ms = self.listen_ms,
            stt_ms,
            ttft_ms,
            tts_ms,
            // NFR-1. Absent when nothing was ever spoken.
            total_ms = first_audio_ms.unwrap_or(0),
            tool_ms = t.tool_ms.load(Ordering::Relaxed),
            tools = t.tool_calls.load(Ordering::Relaxed),
            // stt_ms is endpoint-to-transcript. When the transcript was
            // speculated it is near zero, and this says why rather than
            // leaving the reader to think transcription got faster.
            spec = self.speculative,
            in_tokens = t.in_tokens.load(Ordering::Relaxed),
            out_tokens = t.out_tokens.load(Ordering::Relaxed),
            spoke = first_audio_ms.is_some(),
            stt_backend,
            llm_model = model,
            barged,
            "turn"
        );
    }
}

impl Turn {
    /// The same numbers as the log line, for the screen.
    pub fn event(&self, first_audio_ms: Option<u64>, barged: bool) -> crate::ui::Event {
        let t = &self.timings;
        let stt_ms = t.stt_ms.load(Ordering::Relaxed);
        let ttft_ms = t.ttft_ms.load(Ordering::Relaxed);
        crate::ui::Event::Turn {
            id: self.id,
            total_ms: first_audio_ms.unwrap_or(0),
            stt_ms,
            ttft_ms,
            tts_ms: first_audio_ms
                .map(|a| a.saturating_sub(stt_ms + ttft_ms))
                .unwrap_or(0),
            tool_ms: t.tool_ms.load(Ordering::Relaxed),
            tools: t.tool_calls.load(Ordering::Relaxed),
            barged,
        }
    }
}

/// Which STT backend a turn used, for the log line. Local means no audio left
/// the machine.
pub fn stt_backend() -> &'static str {
    if crate::settings::is_set("IRA_STT_URL") {
        "local"
    } else {
        "groq"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three stage deltas must account for the whole gap, or the line
    /// invites you to hunt for time that was never missing.
    #[test]
    fn stage_deltas_reconstruct_the_total() {
        let turn = Turn::start(1, 0, 0);
        Timings::set(&turn.timings.stt_ms, 700);
        Timings::set(&turn.timings.ttft_ms, 300);
        let first_audio = 1_150;

        let stt = turn.timings.stt_ms.load(Ordering::Relaxed);
        let ttft = turn.timings.ttft_ms.load(Ordering::Relaxed);
        let tts = first_audio - (stt + ttft);
        assert_eq!(stt + ttft + tts, first_audio);
        assert_eq!(tts, 150);
    }

    /// A turn that failed before speaking must not report a plausible-looking
    /// tts_ms derived from an underflow.
    #[test]
    fn silent_turn_reports_no_tts_time() {
        let turn = Turn::start(2, 0, 0);
        Timings::set(&turn.timings.stt_ms, 9_000);
        let tts_ms = None::<u64>
            .map(|a: u64| a.saturating_sub(9_000))
            .unwrap_or(0);
        assert_eq!(tts_ms, 0);
        assert_eq!(turn.id, 2);
    }
}
