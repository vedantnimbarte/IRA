//! `ira.toml`: which MCP servers to connect to, and what IRA believes about
//! their tools.
//!
//! The file is optional. Without it IRA runs with its built-ins and nothing
//! else, which is the state P3 shipped in.
//!
//! The shape deliberately mirrors Wingman's server block so an entry can be
//! copied between them, but the per-tool policy is IRA's own and has no
//! equivalent there -- see [`ToolPolicy`].

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub mcp: Mcp,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mcp {
    /// `[[mcp.server]]` blocks.
    #[serde(default)]
    pub server: Vec<Server>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub name: String,
    /// `stdio` spawns a child process; `http` uses Streamable-HTTP.
    pub transport: String,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Tools to expose, by their server-side name. Empty means all of them.
    ///
    /// Every schema is sent to the model on every round, and a turn that calls
    /// a tool has two rounds, so a server offering sixteen tools puts sixteen
    /// schemas in front of the model twice per turn. In a loop measured in
    /// hundreds of milliseconds that is worth choosing deliberately.
    #[serde(default)]
    pub only: Vec<String>,
    /// Per-tool policy, keyed by server-side tool name.
    #[serde(default)]
    pub tools: BTreeMap<String, ToolPolicy>,
}

/// What IRA believes about one tool, regardless of what the server says.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicy {
    /// Whether running it changes anything. Absent means yes.
    pub mutates: Option<bool>,
    /// `fast`, `slow` or `background`. Absent means slow.
    pub latency: Option<String>,
    /// The question asked before running it.
    pub confirm: Option<String>,
}

impl Server {
    /// The policy for one tool.
    ///
    /// Unlisted tools are treated as writes. A server's own description of
    /// itself is never consulted: a tool that declares itself harmless and is
    /// not would otherwise walk straight through the confirmation gate. Being
    /// asked about a harmless tool is a moment's irritation; the other error is
    /// a sent email. See docs/decisions/0004.
    pub fn policy(&self, tool: &str) -> ToolPolicy {
        self.tools.get(tool).cloned().unwrap_or_default()
    }

    /// Whether a tool should be exposed to the model at all.
    pub fn exposes(&self, tool: &str) -> bool {
        self.only.is_empty() || self.only.iter().any(|t| t == tool)
    }
}

/// Loads `ira.toml`, or `IRA_CONFIG` if set.
///
/// A missing file is not an error -- IRA is useful without servers. A malformed
/// one is: silently ignoring a typo in a `mutates` line would turn the
/// confirmation gate off without saying so.
pub fn load() -> Result<Config> {
    let path = PathBuf::from(std::env::var("IRA_CONFIG").unwrap_or_else(|_| "ira.toml".into()));
    if !path.is_file() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let cfg: Config = toml::from_str(&text)
        .with_context(|| format!("parse {}", path.display()))?;
    tracing::info!(path = %path.display(), servers = cfg.mcp.server.len(), "config");
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Config {
        toml::from_str(s).expect("valid config")
    }

    #[test]
    fn an_unlisted_tool_is_assumed_to_change_things() {
        let cfg = parse(
            r#"
            [[mcp.server]]
            name = "calendar"
            transport = "stdio"
            command = "mcp-calendar"

            [mcp.server.tools]
            list_events = { mutates = false, latency = "fast" }
            "#,
        );
        let server = &cfg.mcp.server[0];

        assert_eq!(server.policy("list_events").mutates, Some(false));
        // The dangerous default: anything we have not vouched for asks first.
        assert_eq!(server.policy("delete_everything").mutates, None);
        assert_eq!(server.policy("delete_everything").latency, None);
    }

    #[test]
    fn an_empty_only_list_exposes_everything() {
        let cfg = parse(
            r#"
            [[mcp.server]]
            name = "kortex"
            transport = "http"
            url = "http://127.0.0.1:8765"
            "#,
        );
        let server = &cfg.mcp.server[0];
        assert!(server.exposes("anything"));

        let cfg = parse(
            r#"
            [[mcp.server]]
            name = "kortex"
            transport = "http"
            url = "http://127.0.0.1:8765"
            only = ["recall", "remember"]
            "#,
        );
        let server = &cfg.mcp.server[0];
        assert!(server.exposes("recall"));
        assert!(!server.exposes("delete_org"), "only-list did not exclude anything");
    }

    /// A typo in a policy key must fail loudly. Ignoring it would silently
    /// disarm the confirmation gate for that tool.
    #[test]
    fn a_misspelled_key_is_rejected() {
        let bad = r#"
            [[mcp.server]]
            name = "x"
            transport = "stdio"
            command = "y"

            [mcp.server.tools]
            send = { mutate = false }
            "#;
        assert!(toml::from_str::<Config>(bad).is_err(), "typo was accepted");
    }

    #[test]
    fn no_config_file_is_not_an_error() {
        // The default is a config with no servers, not a failure.
        let cfg = Config::default();
        assert!(cfg.mcp.server.is_empty());
    }
}
