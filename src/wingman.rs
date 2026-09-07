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
        // Only the last assistant text is worth reading aloud; the diffs, tool
        // calls and verification output belong on the screen.
        let mut last_text = String::new();
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
                    let Some(data) = line.trim().strip_prefix("data:") else {
                        continue;
                    };
                    let Ok(v) = serde_json::from_str::<Value>(data.trim()) else {
                        continue;
                    };
                    if let Some(t) = v["text"].as_str().or_else(|| v["delta"]["text"].as_str()) {
                        last_text.push_str(t);
                    }
                    if v["type"] == "turn.completed" || v["type"] == "turn.finished" {
                        if let Some(s) = v["summary"].as_str() {
                            last_text = s.to_string();
                        }
                    }
                }
            }
        }

        tracing::info!(events, "wingman turn finished");
        Ok(if last_text.trim().is_empty() {
            ToolOutcome::Answer(format!("Wingman finished after {events} steps."))
        } else {
            ToolOutcome::Answer(last_text.trim().to_string())
        })
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

    /// Without IRA_WINGMAN_URL there is nothing to connect to, and the tool
    /// must not be offered to the model.
    #[tokio::test]
    async fn absent_configuration_yields_no_tool() {
        std::env::remove_var("IRA_WINGMAN_URL");
        assert!(Wingman::connect(&reqwest::Client::new()).await.is_none());
    }
}
