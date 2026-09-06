//! Streaming LLM call, chunked into sentences.
//!
//! Sentences, not tokens, are the unit that matters here: TTS can start speaking
//! the first sentence while the model is still writing the second. That overlap
//! is most of the perceived latency win in a voice loop.
//!
//! Anthropic direct by default. Set `IRA_LLM_URL` to talk to anything speaking
//! the OpenAI chat-completions wire format instead -- OpenRouter, LM Studio,
//! Ollama, vLLM, llama.cpp:
//!
//!   $env:IRA_LLM_URL   = "https://openrouter.ai/api/v1/chat/completions"
//!   $env:IRA_LLM_KEY   = "sk-or-..."                    # omit for a local server
//!   $env:IRA_LLM_MODEL = "anthropic/claude-sonnet-4.5"  # provider's own id
//!
//! Only three things differ between the two: where the system prompt goes, the
//! auth header, and where the text sits in each SSE frame.

use crate::metrics::Timings;
use crate::tool::{Host, Latency, ToolCtx, ToolOutcome};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use futures_util::StreamExt;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const MODEL: &str = "claude-sonnet-5";

/// Brevity is the whole personality. An assistant that reads paragraphs aloud is
/// unusable no matter how good the answer is.
pub const SYSTEM: &str = "You are IRA, a voice assistant. You are being spoken to \
and your reply is read aloud, so answer in at most two short sentences. No \
markdown, no lists, no code blocks, no emoji. If the answer genuinely needs more \
room, give the one-line version and offer to go into detail if they ask. When you \
need a tool, call it without saying anything first -- speech cannot be taken back, \
so never start a sentence you might have to abandon.";

/// Spoken while a `Slow` tool runs. Fixed, because a filler that needs a model
/// round trip to produce defeats its own purpose.
const FILLER: &str = "Let me check.";

/// Rounds of tool-calling before IRA answers with whatever it has.
///
/// ponytail: two tools deep is plenty for a voice turn, and each round is a
/// whole model round trip the user waits through. Raise it when something
/// genuinely needs a chain, not before.
const MAX_ROUNDS: usize = 3;

/// A tool call accumulated across streaming frames.
#[derive(Default)]
struct Call {
    id: String,
    name: String,
    /// Raw JSON, arriving in pieces.
    args: String,
}

impl Call {
    fn ready(&self) -> bool {
        !self.name.is_empty()
    }

    fn args(&self) -> Value {
        serde_json::from_str(&self.args).unwrap_or_else(|_| json!({}))
    }
}

/// Folds one SSE frame into an accumulating tool call.
///
/// Both formats dribble the arguments in as text fragments across many frames,
/// so this appends rather than assigns. Returns true if the frame carried
/// tool-call data, which is how the caller knows a call is in progress.
fn fold_tool_call(v: &Value, openai: bool, call: &mut Call) -> bool {
    if openai {
        let Some(tc) = v["choices"][0]["delta"]["tool_calls"].get(0) else {
            return false;
        };
        if let Some(id) = tc["id"].as_str() {
            call.id.push_str(id);
        }
        if let Some(name) = tc["function"]["name"].as_str() {
            call.name.push_str(name);
        }
        if let Some(args) = tc["function"]["arguments"].as_str() {
            call.args.push_str(args);
        }
        return true;
    }

    // Anthropic opens a content block for the call, then streams the input as
    // partial JSON.
    if v["type"] == "content_block_start" && v["content_block"]["type"] == "tool_use" {
        call.id = v["content_block"]["id"].as_str().unwrap_or_default().to_string();
        call.name = v["content_block"]["name"].as_str().unwrap_or_default().to_string();
        return true;
    }
    if v["type"] == "content_block_delta" && v["delta"]["type"] == "input_json_delta" {
        if let Some(frag) = v["delta"]["partial_json"].as_str() {
            call.args.push_str(frag);
        }
        return true;
    }
    false
}

/// Tool definitions in whichever shape the endpoint expects.
fn tool_defs(host: &Host, openai: bool) -> Vec<Value> {
    host.specs()
        .into_iter()
        .map(|s| {
            if openai {
                json!({
                    "type": "function",
                    "function": {
                        "name": s.name,
                        "description": s.description,
                        "parameters": s.schema,
                    }
                })
            } else {
                json!({
                    "name": s.name,
                    "description": s.description,
                    "input_schema": s.schema,
                })
            }
        })
        .collect()
}

/// The two messages that record a completed tool call: what IRA asked for, and
/// what came back. Appended to the conversation before asking again.
fn tool_exchange(call: &Call, result: &str, openai: bool) -> (Value, Value) {
    if openai {
        (
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": call.id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.args },
                }],
            }),
            json!({ "role": "tool", "tool_call_id": call.id, "content": result }),
        )
    } else {
        (
            json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": call.args(),
                }],
            }),
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": call.id,
                    "content": result,
                }],
            }),
        )
    }
}

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

/// The text carried by one SSE frame, for whichever wire format is in use.
///
/// Both are quiet about frames that carry no text -- Anthropic's `message_start`,
/// OpenAI's opening role-only delta -- so `None` is routine, not an error.
fn delta_text(v: &serde_json::Value, openai: bool) -> Option<&str> {
    if openai {
        v["choices"][0]["delta"]["content"].as_str()
    } else if v["type"] == "content_block_delta" {
        v["delta"]["text"].as_str()
    } else {
        None
    }
}

/// Streams the reply, sending each finished sentence to `out`.
/// Returns the full text. Cancelling drops the HTTP stream mid-flight.
pub async fn stream(
    client: &reqwest::Client,
    history: &[(String, String)],
    user: &str,
    out: mpsc::Sender<(String, CancellationToken)>,
    cancel: CancellationToken,
    timings: &Timings,
    host: &Host,
) -> Result<String> {
    let url = std::env::var("IRA_LLM_URL").ok();
    let openai = url.is_some();

    let mut messages = Vec::new();
    // OpenAI carries the system prompt as the first message; Anthropic takes it
    // as a top-level field.
    if openai {
        messages.push(json!({"role": "system", "content": SYSTEM}));
    }
    for (u, a) in history {
        messages.push(json!({"role": "user", "content": u}));
        messages.push(json!({"role": "assistant", "content": a}));
    }
    messages.push(json!({"role": "user", "content": user}));

    let tools = (!host.is_empty()).then(|| tool_defs(host, openai));
    let mut said = String::new();

    for round in 0..MAX_ROUNDS {
        let r = one_round(
            client, &url, openai, &messages, tools.as_ref(), &out, &cancel, timings, round == 0,
        )
        .await?;

        let (call, before) = match r {
            Round::Text(text) => {
                said.push_str(&text);
                return Ok(said);
            }
            Round::Tool(call, before) => (call, before),
        };
        said.push_str(&before);

        // A holding phrase, before the wait rather than after it.
        if host.latency(&call.name) == Some(Latency::Slow)
            && out.send((FILLER.into(), cancel.clone())).await.is_err()
        {
            return Ok(said);
        }

        tracing::info!(tool = %call.name, args = %call.args, "calling tool");
        let started = Instant::now();
        let ctx = ToolCtx { transcript: user.to_string(), cancel: cancel.clone() };
        let outcome = host.call(&call.name, call.args(), &ctx).await;
        timings.tool_ms.fetch_add(
            started.elapsed().as_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        timings
            .tool_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // A refusal cancels the turn from the loop's side, so there is nothing
        // left to ask the model about.
        if cancel.is_cancelled() {
            return Ok(said);
        }

        let result = match outcome {
            Ok(ToolOutcome::Answer(s)) => s,
            Ok(ToolOutcome::Silent) => "Done.".into(),
            Ok(ToolOutcome::Started(id)) => {
                format!("Started as job {id}. It will report when it finishes.")
            }
            // The model phrases the failure, which keeps it inside the
            // conversation rather than ending the turn on a canned line.
            Err(e) => {
                tracing::error!(tool = %call.name, "tool failed: {e}");
                format!("That tool failed: {e}. Tell the user briefly.")
            }
        };

        let (assistant, tool_msg) = tool_exchange(&call, &result, openai);
        messages.push(assistant);
        messages.push(tool_msg);
    }

    tracing::warn!(MAX_ROUNDS, "tool rounds exhausted");
    Ok(said)
}

/// The outcome of one request: either the model answered, or it wants a tool.
enum Round {
    Text(String),
    /// The call, plus any text the model emitted before asking for it.
    Tool(Call, String),
}

/// Issues one request and streams it, speaking sentences as they complete.
#[allow(clippy::too_many_arguments)]
async fn one_round(
    client: &reqwest::Client,
    url: &Option<String>,
    openai: bool,
    messages: &[Value],
    tools: Option<&Vec<Value>>,
    out: &mpsc::Sender<(String, CancellationToken)>,
    cancel: &CancellationToken,
    timings: &Timings,
    mark_ttft: bool,
) -> Result<Round> {
    let mut body = json!({
        "model": std::env::var("IRA_LLM_MODEL").unwrap_or_else(|_| MODEL.into()),
        "max_tokens": 300,
        "stream": true,
        "messages": messages,
    });
    if !openai {
        body["system"] = SYSTEM.into();
    }
    if let Some(tools) = tools {
        body["tools"] = json!(tools);
    }

    let req = match url {
        // A local server usually wants no key at all, so an absent one is not
        // an error here the way a missing ANTHROPIC_API_KEY is.
        Some(url) => match std::env::var("IRA_LLM_KEY") {
            Ok(key) => client.post(url).bearer_auth(key),
            Err(_) => client.post(url),
        },
        None => client
            .post("https://api.anthropic.com/v1/messages")
            .header("anthropic-version", "2023-06-01")
            .header(
                "x-api-key",
                std::env::var("ANTHROPIC_API_KEY")
                    .map_err(|_| anyhow!("ANTHROPIC_API_KEY not set"))?,
            ),
    };

    let resp = req.json(&body).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("llm {}: {}", resp.status(), resp.text().await?));
    }

    // Time to first token separates the model's latency from ours. Without it a
    // slow turn is just slow, with nothing to point at. A tool call counts: it
    // is the first thing the model produced, even though it is not speech.
    let asked = Instant::now();
    let mut first_token = false;
    let mut mark = |timings: &Timings| {
        if mark_ttft && !first_token {
            first_token = true;
            Timings::set(&timings.ttft_ms, asked.elapsed().as_millis() as u64);
        }
    };

    let mut stream = resp.bytes_stream();
    let mut sse = String::new();
    let mut buf = String::new();
    let mut full = String::new();
    let mut call = Call::default();

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
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };

            if fold_tool_call(&v, openai, &mut call) {
                mark(timings);
                continue;
            }

            if let Some(t) = delta_text(&v, openai) {
                mark(timings);
                full.push_str(t);
                buf.push_str(t);
                while let Some(s) = split_sentence(&mut buf) {
                    if !s.is_empty() && out.send((s, cancel.clone())).await.is_err() {
                        return Ok(Round::Text(full));
                    }
                }
            }
        }
    }

    // Whatever is left is a complete thought even without a full stop.
    let tail = buf.trim().to_string();
    if !tail.is_empty() && !cancel.is_cancelled() {
        let _ = out.send((tail, cancel.clone())).await;
    }

    if call.ready() {
        Ok(Round::Tool(call, full))
    } else {
        Ok(Round::Text(full))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    /// Real frame shapes from both wire formats. The two disagree about where
    /// the text lives and about which frames carry any, so reading one with the
    /// other's rules yields a silent empty reply rather than an error.
    #[test]
    fn reads_text_from_either_wire_format() {
        let openai = json(
            r#"{"choices":[{"delta":{"content":"Hello"},"index":0}]}"#,
        );
        let anthropic = json(
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}"#,
        );
        assert_eq!(delta_text(&openai, true), Some("Hello"));
        assert_eq!(delta_text(&anthropic, false), Some("Hello"));

        // Each format's opening frame carries no text and must not panic or
        // yield a spurious empty string.
        assert_eq!(
            delta_text(&json(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#), true),
            None
        );
        assert_eq!(
            delta_text(&json(r#"{"type":"message_start","message":{"id":"x"}}"#), false),
            None
        );
        // Neither may read the other's frame, which is the failure that would
        // otherwise show up as IRA silently saying nothing.
        assert_eq!(delta_text(&anthropic, true), None);
        assert_eq!(delta_text(&openai, false), None);
    }

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
