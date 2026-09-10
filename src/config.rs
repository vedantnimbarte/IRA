//! Where the MCP server list comes from, and the one-time import of the file it
//! used to come from.
//!
//! Servers live in `ira.local.db` so the settings window can edit them while
//! IRA runs ([0017](../docs/decisions/0017-servers-and-skills-are-configured-in-the-window.md)).
//! `ira.toml` is read exactly once, on the first start after upgrading, and
//! never again -- there is one source of truth, and a file that keeps being
//! re-read would fight the window every restart.
//!
//! The TOML types below exist only for that import. They are the shape 0002
//! shipped, kept deliberately strict: `deny_unknown_fields` means a typo in a
//! `mutates` line fails the import loudly rather than silently importing a
//! server with its confirmation gate off.

use crate::db;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Set in `settings` once the import has run, so it runs once even if the file
/// is still there and even if the user then deleted every server it brought in.
const IMPORTED: &str = "_ira_toml_imported";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Toml {
    #[serde(default)]
    mcp: Mcp,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mcp {
    #[serde(default)]
    server: Vec<TomlServer>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlServer {
    name: String,
    transport: String,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    only: Vec<String>,
    #[serde(default)]
    tools: BTreeMap<String, TomlPolicy>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlPolicy {
    mutates: Option<bool>,
    latency: Option<String>,
    confirm: Option<String>,
}

/// The configured servers, from the database.
pub fn servers() -> Vec<db::Server> {
    match db::servers() {
        Ok(s) => s,
        // A database IRA cannot read must not stop her answering questions. She
        // is useful with no tools at all; she is useless if she will not start.
        Err(e) => {
            tracing::error!("could not read the server list: {e:#}");
            Vec::new()
        }
    }
}

/// Imports `ira.toml` into the database, once, on the first start after the
/// window became the way to edit servers.
///
/// A missing file is the normal case. A malformed one is reported and skipped:
/// this runs at start-up, and refusing to boot over a stray quote in a config
/// file that is no longer the source of truth would be the wrong trade.
pub fn import_toml_once() {
    let path = match std::env::var("IRA_CONFIG") {
        Ok(p) => PathBuf::from(p),
        Err(_) => crate::paths::in_data("ira.toml"),
    };
    if !path.is_file() {
        return;
    }
    match db::settings_all() {
        Ok(s) if s.contains_key(IMPORTED) => return,
        Ok(_) => {}
        Err(e) => {
            tracing::error!("could not check whether {} was imported: {e:#}", path.display());
            return;
        }
    }

    match import(&path) {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            servers = n,
            path = %path.display(),
            "imported into ira.local.db -- the file is no longer read, edit servers in the settings window"
        ),
        Err(e) => {
            tracing::error!("could not import {}: {e:#}", path.display());
            // Deliberately not marked as imported: a file that failed to parse
            // is one the user will want to fix and have picked up next time.
            return;
        }
    }

    if let Err(e) = db::settings_set(IMPORTED, "1") {
        tracing::error!("could not record the import, it will run again: {e:#}");
    }
}

fn import(path: &std::path::Path) -> Result<usize> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let cfg: Toml =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;

    for s in &cfg.mcp.server {
        db::server_set(&db::Server {
            name: s.name.clone(),
            transport: s.transport.clone(),
            command: s.command.clone(),
            args: s.args.clone(),
            url: s.url.clone(),
            headers: s.headers.clone(),
            enabled: true,
            tools: BTreeMap::new(),
        })?;

        // `only` was a separate list; in the database it is the `exposed` flag
        // on each tool. A tool named in `only` but with no policy block still
        // needs a row, or it would be exposed by the default and the list would
        // have been silently dropped.
        let named: Vec<&String> = s.only.iter().chain(s.tools.keys()).collect();
        for tool in named {
            let p = s.tools.get(tool).cloned().unwrap_or_default();
            db::tool_policy_set(
                &s.name,
                tool,
                &db::ToolPolicy {
                    exposed: s.only.is_empty() || s.only.contains(tool),
                    mutates: p.mutates,
                    latency: p.latency.clone(),
                    confirm: p.confirm.clone(),
                },
            )?;
        }
    }
    Ok(cfg.mcp.server.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Toml {
        toml::from_str(s).expect("valid config")
    }

    /// A typo in a policy key must fail the import loudly. Ignoring it would
    /// silently bring in a server with the confirmation gate off for that tool.
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
        assert!(toml::from_str::<Toml>(bad).is_err(), "typo was accepted");
    }

    /// `only` and `tools` are two ways of naming the same tools, and the import
    /// has to reconcile them into one `exposed` flag per row. A tool in `only`
    /// with no policy block still needs a row; a tool with a policy block but
    /// not in a non-empty `only` must come out hidden.
    #[test]
    fn only_and_tools_become_one_exposed_flag() {
        let cfg = parse(
            r#"
            [[mcp.server]]
            name = "kortex"
            transport = "http"
            url = "http://127.0.0.1:8765"
            only = ["recall", "remember"]

            [mcp.server.tools]
            recall = { mutates = false, latency = "fast" }
            delete_org = { mutates = true }
            "#,
        );
        let s = &cfg.mcp.server[0];

        let named: Vec<&String> = s.only.iter().chain(s.tools.keys()).collect();
        let exposed = |t: &str| s.only.is_empty() || s.only.contains(&t.to_string());

        assert!(named.iter().any(|n| *n == "remember"), "a bare `only` entry needs a row");
        assert!(exposed("recall"));
        assert!(exposed("remember"));
        assert!(!exposed("delete_org"), "a tool outside `only` must come out hidden");
        assert_eq!(s.tools["recall"].mutates, Some(false));
    }

    /// No file, or a file with no servers, is not an error.
    #[test]
    fn an_empty_config_is_not_an_error() {
        assert!(parse("").mcp.server.is_empty());
    }
}
