//! A record of what was said, one JSON object per line.
//!
//! JSONL rather than a database or a prose log: it appends without rewriting,
//! survives being truncated mid-write, and `tail -f` reads it while IRA is still
//! talking. Anything that wants structure can parse it; anything that wants to
//! read it can just read it.
//!
//! This is text, not audio. Audio is still buffered for one utterance and
//! discarded -- but a conversation on disk is a change in posture from a
//! process that kept nothing, so the path is logged at start-up rather than
//! left to be discovered, and `IRA_TRANSCRIPT=off` turns it off.

use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;

/// One turn, as said rather than as timed.
///
/// Deliberately carries no latency. A reply is recorded when the model
/// finishes, which is before a word of it has been spoken, so anything measured
/// here would be read too early and would mostly be zero. Timing lives in the
/// `turn` log line, which is emitted when the turn is actually over.
#[derive(Serialize)]
pub struct Entry<'a> {
    /// Seconds since the Unix epoch. No date formatting, and so no timezone to
    /// be wrong about later.
    pub at: u64,
    pub turn: u64,
    pub user: &'a str,
    pub ira: &'a str,
    pub tools: u64,
    /// Zero unless the endpoint volunteered a count; see `llm::fold_usage`.
    pub in_tokens: u64,
    pub out_tokens: u64,
}

pub struct Transcript {
    path: Option<PathBuf>,
}

impl Transcript {
    /// `IRA_TRANSCRIPT` sets the path; `off` disables it.
    pub fn open() -> Self {
        let setting = std::env::var("IRA_TRANSCRIPT").unwrap_or_default();
        if setting == "off" {
            tracing::info!("transcript off");
            return Self { path: None };
        }
        let path = if setting.is_empty() {
            PathBuf::from("transcript.jsonl")
        } else {
            PathBuf::from(setting)
        };
        // Said out loud at start-up: a file recording every conversation should
        // not be something you find out about by accident.
        tracing::info!(path = %path.display(), "transcript");
        Self { path: Some(path) }
    }

    pub fn append(&self, entry: &Entry) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(line) = serde_json::to_string(entry) else {
            return;
        };
        // A transcript that cannot be written must not interrupt a
        // conversation; it is a record of the work, not the work.
        let write = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = write {
            tracing::error!(?e, path = %path.display(), "transcript write failed");
        }
    }
}

/// Seconds since the epoch, or 0 on a machine whose clock predates it.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry<'a>(user: &'a str, ira: &'a str) -> Entry<'a> {
        Entry {
            at: 1_700_000_000,
            turn: 1,
            user,
            ira,
            tools: 0,
            in_tokens: 0,
            out_tokens: 0,
        }
    }

    /// One line per turn, and each line valid on its own -- that is the whole
    /// reason for the format, and a newline inside a field would break it.
    #[test]
    fn each_turn_is_one_parseable_line() {
        let path = std::env::temp_dir().join("ira-transcript-test.jsonl");
        let _ = std::fs::remove_file(&path);
        let t = Transcript { path: Some(path.clone()) };

        t.append(&entry("what time is it", "Just gone four."));
        // A reply containing a newline must not become two records.
        t.append(&entry("read it back", "One.\nTwo."));

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "got {text:?}");
        for line in lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert_eq!(v["turn"], 1);
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Turned off, it must not touch the disk at all.
    #[test]
    fn off_writes_nothing() {
        let t = Transcript { path: None };
        t.append(&entry("hello", "hi"));
        // Nothing to assert but the absence of a panic and of a path; the point
        // is that `append` on a disabled transcript is a no-op, not an error.
        assert!(t.path.is_none());
    }

    /// A path that cannot be written must not take the conversation down.
    #[test]
    fn an_unwritable_path_is_survivable() {
        let t = Transcript {
            path: Some(PathBuf::from("no-such-dir/nested/deeper/transcript.jsonl")),
        };
        t.append(&entry("hello", "hi"));
    }
}
