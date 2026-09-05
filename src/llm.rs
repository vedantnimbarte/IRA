//! Streaming Anthropic call, chunked into sentences.
//!
//! Sentences, not tokens, are the unit that matters here: TTS can start speaking
//! the first sentence while the model is still writing the second. That overlap
//! is most of the perceived latency win in a voice loop.

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const MODEL: &str = "claude-sonnet-5";

/// Brevity is the whole personality. An assistant that reads paragraphs aloud is
/// unusable no matter how good the answer is.
pub const SYSTEM: &str = "You are IRA, a voice assistant. You are being spoken to \
and your reply is read aloud, so answer in at most two short sentences. No \
markdown, no lists, no code blocks, no emoji. If the answer genuinely needs more \
room, give the one-line version and say you have put the detail on screen.";

/// Flush at sentence ends, or at a word boundary if one sentence runs long.
fn split_sentence(buf: &mut String) -> Option<String> {
    let bytes = buf.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        if matches!(c, b'.' | b'!' | b'?') {
            // Require whitespace (or end of buffer) after the mark, so "3.50"
            // and "e.g." do not each become their own utterance.
            let boundary = bytes.get(i + 1).is_none_or(|n| n.is_ascii_whitespace());
            if boundary && i >= 1 {
                let s: String = buf.drain(..=i).collect();
                return Some(s.trim().to_string());
            }
        }
    }
    if buf.len() > 160 {
        if let Some(sp) = buf.rfind(' ') {
            let s: String = buf.drain(..=sp).collect();
            return Some(s.trim().to_string());
        }
    }
    None
}

/// Streams the reply, sending each finished sentence to `out`.
/// Returns the full text. Cancelling drops the HTTP stream mid-flight.
pub async fn stream(
    client: &reqwest::Client,
    history: &[(String, String)],
    user: &str,
    out: mpsc::Sender<String>,
    cancel: CancellationToken,
) -> Result<String> {
    let key =
        std::env::var("ANTHROPIC_API_KEY").map_err(|_| anyhow!("ANTHROPIC_API_KEY not set"))?;

    let mut messages = Vec::new();
    for (u, a) in history {
        messages.push(serde_json::json!({"role": "user", "content": u}));
        messages.push(serde_json::json!({"role": "assistant", "content": a}));
    }
    messages.push(serde_json::json!({"role": "user", "content": user}));

    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": MODEL,
            "max_tokens": 300,
            "system": SYSTEM,
            "stream": true,
            "messages": messages,
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(anyhow!("llm {}: {}", resp.status(), resp.text().await?));
    }

    let mut stream = resp.bytes_stream();
    let mut sse = String::new();
    let mut buf = String::new();
    let mut full = String::new();

    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            c = stream.next() => match c {
                Some(c) => c?,
                None => break,
            },
        };
        sse.push_str(&String::from_utf8_lossy(&chunk));

        // SSE frames are newline-delimited; keep any partial trailing line.
        while let Some(nl) = sse.find('\n') {
            let line: String = sse.drain(..=nl).collect();
            let Some(data) = line.trim().strip_prefix("data: ") else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            if v["type"] == "content_block_delta" {
                if let Some(t) = v["delta"]["text"].as_str() {
                    full.push_str(t);
                    buf.push_str(t);
                    while let Some(s) = split_sentence(&mut buf) {
                        if !s.is_empty() && out.send(s).await.is_err() {
                            return Ok(full);
                        }
                    }
                }
            }
        }
    }

    let tail = buf.trim().to_string();
    if !tail.is_empty() && !cancel.is_cancelled() {
        let _ = out.send(tail).await;
    }
    Ok(full)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_sentence_end() {
        let mut b = String::from("Hello there. How are");
        assert_eq!(split_sentence(&mut b).as_deref(), Some("Hello there."));
        assert_eq!(b, " How are");
        assert_eq!(split_sentence(&mut b), None);
    }

    #[test]
    fn does_not_split_decimals() {
        let mut b = String::from("It costs 3.50 dollars");
        assert_eq!(split_sentence(&mut b), None);
    }

    #[test]
    fn long_run_on_flushes_at_a_word_boundary() {
        // Trailing partial word: the flush must stop at the last space and
        // leave the incomplete word behind for the next delta.
        let mut b = format!("{}partial", "word ".repeat(40));
        let s = split_sentence(&mut b).expect("should flush");
        assert!(s.ends_with("word"), "got {s:?}");
        assert_eq!(b, "partial");
    }
}
