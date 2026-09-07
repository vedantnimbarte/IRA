//! Wingman, reached over `wingman serve`'s HTTP API.
//!
//! Wingman is a terminal coding agent. It is an MCP *client*, not a server, so
//! unlike every other capability it cannot arrive through `mcp.rs` and a line of
//! config -- something has to speak its own API. That something is this file,
//! deliberately: the alternative is a separate MCP shim project existing solely
//! to wrap one daemon, which is more moving parts for the same coupling.
//!
//! See docs/decisions/0007-wingman-over-http-not-as-a-library.md, which chose
//! HTTP over a crate dependency, and 0012 for why this ended up in-tree.
//!
//! The contract is small and taken from a client that already works, ARC's
//! `rust/wingman`:
//!
//! - `GET /v1/health` is the one unauthenticated route, so it answers both
//!   "is a daemon there" and "will it want a token".
//! - `GET /v1/projects` is an allowlist. Nothing outside it is reachable.
//! - `POST /v1/projects/{id}/turns` takes `{prompt, model, mode}` and streams
//!   typed events back. A refused turn is a JSON error, not an empty stream.

use crate::tool::{Latency, Tool, ToolCtx, ToolOutcome, ToolSpec};
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;

/// How long to wait for the daemon to admit it exists.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

pub struct Wingman {
    client: reqwest::Client,
    base: String,
    token: Option<String>,
    project: String,
}

impl Wingman {
    /// Connects if `IRA_WINGMAN_URL` names a reachable daemon.
    ///
    /// Returns `None` rather than an error when it is unset: most people do not
    /// run Wingman, and a tool that is advertised to the model but cannot work
    /// is worse than no tool at all.
    pub async fn connect(client: &reqwest::Client) -> Option<Self> {
        let base = std::env::var("IRA_WINGMAN_URL").ok()?.trim_end_matches('/').to_string();

        let health: Value = match client
            .get(format!("{base}/v1/health"))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(r) => r.json().await.ok()?,
            Err(e) => {
                tracing::error!("wingman unreachable at {base}: {e}");
                return None;
            }
        };

        // An exported-but-empty variable is how a shell says "unset", and
        // `Some("")` here would send `Bearer ` and get a 401 with no clue why.
        let token = std::env::var("IRA_WINGMAN_TOKEN").ok().filter(|t| !t.is_empty());
        if health["auth_required"].as_bool().unwrap_or(false) && token.is_none() {
            tracing::error!("wingman at {base} wants a token; set IRA_WINGMAN_TOKEN");
            return None;
        }

        // Wingman only reaches projects on its own allowlist, so there is no
        // point asking it about anything else.
        let project = match std::env::var("IRA_WINGMAN_PROJECT") {
            Ok(p) => p,
            Err(_) => {
                // error_for_status first: a 401 body parses perfectly well as
                // JSON and then reads as an empty project list, which blames
                // the wrong thing entirely.
                let list: Value = match auth(client.get(format!("{base}/v1/projects")), &token)
                    .timeout(PROBE_TIMEOUT)
                    .send()
                    .await
                    .and_then(|r| r.error_for_status())
                {
                    Ok(r) => r.json().await.ok()?,
                    Err(e) => {
                        tracing::error!("wingman would not list its projects: {e}");
                        return None;
                    }
                };
                let first = list
                    .as_array()
                    .or_else(|| list["projects"].as_array())
                    .and_then(|a| a.first())
                    .and_then(|p| p["id"].as_str())
                    .map(String::from);
                match first {
                    Some(p) => p,
                    None => {
                        tracing::error!("wingman has no projects; set IRA_WINGMAN_PROJECT");
                        return None;
                    }
                }
            }
        };

        tracing::info!(
            version = %health["version"].as_str().unwrap_or("?"),
            %project,
            "wingman connected"
        );
        Some(Self { client: client.clone(), base, token, project })
    }
}

/// What a turn's event stream added up to.
///
/// The event names are `wingman_core::AgentEvent` in snake case —
/// `text_delta`, `thinking_delta`, `tool_start`, `tool_result`, `usage`,
/// `turn_complete`, `verification`, `stop`, `error` — plus an `end` the daemon
/// adds when the child process exits.
///
/// A turn that fails does so **inside a 200 stream**: the provider is
/// unreachable, the key is rejected, the verification gate stays red. Only
/// checking the HTTP status would report every one of those as a success with
/// nothing to say.
#[derive(Default)]
struct Turn {
    text: String,
    verification: Option<String>,
    /// The first error, which is the one that caused the rest.
    error: Option<String>,
    stop: Option<String>,
    exit: Option<i64>,
    stderr: String,
}

impl Turn {
    fn event(&mut self, v: &Value) {
        match v["type"].as_str().unwrap_or_default() {
            "text_delta" => self.text.push_str(v["text"].as_str().unwrap_or_default()),
            // thinking_delta is the model's working-out, not its answer.
            "verification" => {
                let passed = v["passed"].as_bool().unwrap_or(false);
                let summary = v["summary"].as_str().unwrap_or_default();
                self.verification = Some(if passed {
                    format!("Checks passed. {summary}")
                } else {
                    format!("Checks failed. {summary}")
                });
            }
            "stop" => self.stop = v["reason"].as_str().map(String::from),
            "error" => {
                if self.error.is_none() {
                    self.error = Some(v["message"].as_str().unwrap_or("unknown error").into());
                }
            }
            "end" => {
                self.exit = v["exit"].as_i64();
                self.stderr = v["stderr"].as_str().unwrap_or_default().into();
            }
            _ => {}
        }
    }

    fn outcome(self, events: usize) -> Result<ToolOutcome> {
        if let Some(e) = self.error {
            return Err(anyhow!("wingman: {e}"));
        }
        // `end_turn` is the only clean finish. `max_turns`, `max_tokens` and
        // `gate_failed` all mean it stopped short of what was asked, and
        // reading out whatever it had written by then would imply otherwise.
        match self.stop.as_deref() {
            Some("end_turn") | None => {}
            Some(other) => return Err(anyhow!("wingman stopped early: {other}")),
        }
        if let Some(code) = self.exit.filter(|c| *c != 0) {
            let why = self.stderr.lines().next_back().unwrap_or("no output").trim();
            return Err(anyhow!("wingman exited {code}: {why}"));
        }

        let mut out = self.text.trim().to_string();
        if let Some(v) = self.verification {
            // The one part of the machinery worth hearing: it is the difference
            // between "it wrote something" and "it works".
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&v);
        }
        Ok(ToolOutcome::Answer(if out.is_empty() {
            format!("Wingman finished after {events} events with nothing to say.")
        } else {
            out
        }))
    }
}

fn auth(req: reqwest::RequestBuilder, token: &Option<String>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

#[async_trait::async_trait]
impl Tool for Wingman {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "wingman".into(),
            description: "Give a coding task to Wingman, an agent that edits this \
                          project's code, runs its build and its tests. Use it for work \
                          on the codebase, not for questions about it. It takes minutes."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The task, as you would give it to a colleague"
                    }
                },
                "required": ["task"]
            }),
            // It edits files and runs commands. Nothing about that should happen
            // because a sentence was misheard.
            mutates: true,
            latency: Latency::Background,
            confirm: Some("Send that to Wingman?".into()),
        }
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let task = args["task"].as_str().unwrap_or_default().trim();
        if task.is_empty() {
            return Err(anyhow!("no task given"));
        }

        let resp = auth(
            self.client
                .post(format!("{}/v1/projects/{}/turns", self.base, self.project)),
            &self.token,
        )
        .header("Accept", "text/event-stream")
        .json(&json!({ "prompt": task, "model": null, "mode": null }))
        .send()
        .await?;

        // A refused turn -- a busy session, a full queue, a spend ceiling --
        // comes back as ordinary JSON rather than a stream. Reporting "no
        // events" would describe it as silence rather than a refusal.
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("wingman refused the turn ({status}): {}", body.trim()));
        }

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut turn = Turn::default();
        let mut events = 0usize;

        while let Some(chunk) = stream.next().await {
            if ctx.cancel.is_cancelled() {
                // The job outlives the turn that asked for it, so this only
                // fires if the whole loop is going away.
                break;
            }
            // SSE permits either line ending; splitting on one of them means
            // half the servers in the world stream into a buffer that never
            // yields a frame.
            buf.push_str(&String::from_utf8_lossy(&chunk?).replace("\r\n", "\n"));
            while let Some(cut) = buf.find("\n\n") {
                let frame: String = buf.drain(..cut + 2).collect();
                events += 1;
                for line in frame.lines() {
                    // The `event:` name is the payload's own `type`, so the
                    // data line is the whole story and the name is redundant.
                    let Some(data) = line.trim().strip_prefix("data:") else {
                        continue;
                    };
                    if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                        turn.event(&v);
                    }
                }
            }
        }

        tracing::info!(events, stop = ?turn.stop, "wingman turn finished");
        turn.outcome(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wingman() -> Wingman {
        Wingman {
            client: reqwest::Client::new(),
            base: "http://127.0.0.1:1".into(),
            token: None,
            project: "demo".into(),
        }
    }

    /// Sending code changes because a sentence was misheard is the failure this
    /// whole gate exists for, and Wingman is the most expensive way to make it.
    #[test]
    fn a_coding_task_always_asks_first() {
        let spec = wingman().spec();
        assert!(spec.mutates, "wingman edits code and must be confirmed");
        assert!(spec.confirm.is_some());
        assert_eq!(spec.latency, Latency::Background, "a turn takes minutes");
    }

    /// An empty task must not reach the daemon at all.
    #[tokio::test]
    async fn an_empty_task_is_refused_locally() {
        let ctx = ToolCtx {
            transcript: String::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let err = wingman()
            .call(json!({"task": "   "}), &ctx)
            .await
            .expect_err("empty task");
        assert!(err.to_string().contains("no task"), "got {err}");
    }

    /// Feed a turn the events it would have received.
    fn turn(events: &[Value]) -> Result<ToolOutcome> {
        let mut t = Turn::default();
        for e in events {
            t.event(e);
        }
        t.outcome(events.len())
    }

    #[test]
    fn a_finished_turn_is_its_text_and_its_verdict() {
        let out = turn(&[
            json!({"type": "thinking_delta", "text": "the uploader is in src/"}),
            json!({"type": "tool_start", "id": "1", "name": "edit", "input": {}}),
            json!({"type": "text_delta", "text": "Added three retries "}),
            json!({"type": "text_delta", "text": "with backoff."}),
            json!({"type": "verification", "passed": true, "summary": "48 tests, 0 failed."}),
            json!({"type": "stop", "reason": "end_turn"}),
            json!({"type": "end", "exit": 0, "stderr": ""}),
        ])
        .unwrap();
        let ToolOutcome::Answer(text) = out else { panic!("expected an answer") };
        assert_eq!(text, "Added three retries with backoff. Checks passed. 48 tests, 0 failed.");
        assert!(!text.contains("the uploader is in"), "thinking is not the answer");
    }

    /// The failure the real daemon exposed: a turn that cannot reach its
    /// provider still returns **200**, and says so only inside the stream.
    /// Judging by the HTTP status alone reports a dead turn as a success with
    /// nothing to say.
    #[test]
    fn an_error_inside_a_200_stream_is_still_a_failure() {
        let err = turn(&[
            json!({"type": "error", "message": "provider error: openrouter returned 401"}),
            json!({"type": "stop", "reason": "error"}),
            json!({"type": "end", "exit": 1, "stderr": "wingman: no credentials\n"}),
        ])
        .expect_err("an error event must not read as success");
        assert!(err.to_string().contains("401"), "got {err}");
    }

    /// Stopping short is not finishing. Reading out the half a change it
    /// managed, with no mention that the gate is still red, is the worst
    /// possible summary of a failed build.
    #[test]
    fn stopping_short_is_not_finishing() {
        for reason in ["gate_failed", "max_turns", "max_tokens"] {
            let err = turn(&[
                json!({"type": "text_delta", "text": "I changed the parser."}),
                json!({"type": "stop", "reason": reason}),
            ])
            .expect_err("{reason} must not read as a finished turn");
            assert!(err.to_string().contains(reason), "got {err}");
        }
    }

    /// A red gate that reports itself properly still has to be heard.
    #[test]
    fn a_failed_check_is_spoken_not_swallowed() {
        let out = turn(&[
            json!({"type": "text_delta", "text": "Done."}),
            json!({"type": "verification", "passed": false, "summary": "2 tests failed."}),
            json!({"type": "stop", "reason": "end_turn"}),
        ])
        .unwrap();
        let ToolOutcome::Answer(text) = out else { panic!("expected an answer") };
        assert!(text.contains("Checks failed. 2 tests failed."), "got {text}");
    }

    /// Without IRA_WINGMAN_URL there is nothing to connect to, and the tool
    /// must not be offered to the model.
    #[tokio::test]
    async fn absent_configuration_yields_no_tool() {
        std::env::remove_var("IRA_WINGMAN_URL");
        assert!(Wingman::connect(&reqwest::Client::new()).await.is_none());
    }
}
