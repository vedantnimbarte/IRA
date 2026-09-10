//! `ira.local.db`: everything about this machine that IRA can change while she
//! runs.
//!
//! [decisions/0015](../docs/decisions/0015-settings-come-from-the-keyring-not-the-environment.md)
//! put the provider settings here. [0017](../docs/decisions/0017-servers-and-skills-are-configured-in-the-window.md)
//! added the MCP servers, their per-tool policy, and which skills are on --
//! everything the settings window can edit, in one place it can write to.
//!
//! Six tables, none of them large:
//!
//! ```text
//!   settings      name → value          URLs and model ids (keys are in the keyring)
//!   mcp_server    one row per server    transport, command, url, headers
//!   mcp_tool      one row per tool      exposed, mutates, latency, confirm
//!   mcp_env       one row per variable  names only -- values are in the keyring
//!   mcp_oauth     one row per server    client id and scopes -- tokens are in the keyring
//!   skill         one row per file      which are on, and what they say they do
//! ```
//!
//! **Nothing secret is in here.** Every table that touches a credential stores
//! the *name* of one and leaves the value in the OS keyring, which is the same
//! split 0015 made for IRA'"'"'s own keys.
//!
//! **Connections are opened per call.** These are a handful of rows edited by
//! hand in a window; the open costs nothing next to the save it is part of, and
//! there is no pooled handle to keep alive across a failure. If this ever grows
//! a hot path, that is the thing to revisit first.
//!
//! **Bodies are not in here.** A skill's text stays in its `.md` file, which is
//! what an editor and a repository can see -- the row is an index over it.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::BTreeMap;

pub const PATH: &str = "ira.local.db";

/// Opens the database, creating it and its tables if they are not there.
///
/// `CREATE TABLE IF NOT EXISTS` on every open rather than a migration number:
/// four tables that have only ever been added to, and a version column would be
/// ceremony around a schema that fits on a screen. The day a column has to
/// *change* is the day this needs a real migration, and that day is not today.
///
/// **`IF NOT EXISTS` covers a new table, not a new column.** An existing
/// database already has `mcp_server`, so a column added to that statement would
/// silently never appear for anyone who had run IRA before. New data therefore
/// goes in a new table -- which is why per-server environment variables are
/// `mcp_env` rather than a column on `mcp_server`, and it is the better shape
/// anyway.
///
/// ponytail: no schema versioning. The day a column has to change type or
/// meaning is the day this needs real migrations. Until then, add tables.
fn open() -> Result<Connection> {
    let conn = Connection::open(PATH).with_context(|| format!("open {PATH}"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
             name  TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS mcp_server (
             name      TEXT PRIMARY KEY,
             transport TEXT NOT NULL,
             command   TEXT,
             args      TEXT NOT NULL DEFAULT '[]',
             url       TEXT,
             headers   TEXT NOT NULL DEFAULT '{}',
             enabled   INTEGER NOT NULL DEFAULT 1
         );
         CREATE TABLE IF NOT EXISTS mcp_tool (
             server  TEXT NOT NULL,
             tool    TEXT NOT NULL,
             exposed INTEGER NOT NULL DEFAULT 1,
             -- NULL means 'not vouched for', which resolves to a write. The
             -- column is nullable on purpose: absent and false must not be the
             -- same value, or the gate is off for every tool nobody has judged.
             mutates INTEGER,
             latency TEXT,
             confirm TEXT,
             PRIMARY KEY (server, tool)
         );
         CREATE TABLE IF NOT EXISTS skill (
             name        TEXT PRIMARY KEY,
             path        TEXT NOT NULL,
             description TEXT NOT NULL DEFAULT '',
             enabled     INTEGER NOT NULL DEFAULT 1
         );
         -- Which variables a server is given, never their values. A value is a
         -- credential and lives in the OS keyring under IRA/env/<server>/<name>,
         -- for the same reason IRA's own keys are not in this file.
         CREATE TABLE IF NOT EXISTS mcp_env (
             server TEXT NOT NULL,
             name   TEXT NOT NULL,
             PRIMARY KEY (server, name)
         );
         -- Servers that sign in rather than carrying a static header. Only the
         -- client id is here; tokens are in the keyring and refresh themselves.
         CREATE TABLE IF NOT EXISTS mcp_oauth (
             server    TEXT PRIMARY KEY,
             client_id TEXT,
             scopes    TEXT NOT NULL DEFAULT '[]'
         );",
    )
    .context("create the tables")?;
    Ok(conn)
}

/// Whether the database file exists yet. A machine that has never saved
/// anything is the normal case, and opening would create an empty one.
fn exists() -> bool {
    std::path::Path::new(PATH).exists()
}

// ---------------------------------------------------------------- settings --

pub fn settings_all() -> Result<BTreeMap<String, String>> {
    if !exists() {
        return Ok(BTreeMap::new());
    }
    let conn = open()?;
    let mut q = conn.prepare("SELECT name, value FROM settings")?;
    let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn settings_set(name: &str, value: &str) -> Result<()> {
    open()?
        .execute(
            "INSERT INTO settings (name, value) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET value = excluded.value",
            (name, value),
        )
        .with_context(|| format!("save {name}"))?;
    Ok(())
}

pub fn settings_delete(name: &str) -> Result<()> {
    open()?
        .execute("DELETE FROM settings WHERE name = ?1", (name,))
        .with_context(|| format!("clear {name}"))?;
    Ok(())
}

// -------------------------------------------------------------- mcp servers --

/// One configured server, as stored. The shape `mcp.rs` connects with.
#[derive(Debug, Clone, Default)]
pub struct Server {
    pub name: String,
    /// `stdio` spawns a child process; `http` uses Streamable-HTTP.
    pub transport: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    /// A server the user has turned off stays configured and is not connected.
    pub enabled: bool,
    /// Per-tool policy, keyed by the server's own tool name. Only tools someone
    /// has actually looked at are in here.
    pub tools: BTreeMap<String, ToolPolicy>,
}

/// What IRA believes about one tool, regardless of what the server says.
#[derive(Debug, Clone, Default)]
pub struct ToolPolicy {
    /// Whether to offer it to the model at all.
    pub exposed: bool,
    /// Whether running it changes anything. `None` means nobody has said, which
    /// resolves to yes -- see [`Server::policy`].
    pub mutates: Option<bool>,
    /// `fast`, `slow` or `background`. `None` means slow.
    pub latency: Option<String>,
    /// The question asked before running it.
    pub confirm: Option<String>,
}

impl Server {
    /// The policy for one tool.
    ///
    /// A tool nobody has judged is treated as a write. A server's own
    /// description of itself is never consulted: a tool that declares itself
    /// harmless and is not would otherwise walk straight through the
    /// confirmation gate. See docs/decisions/0004.
    pub fn policy(&self, tool: &str) -> ToolPolicy {
        self.tools.get(tool).cloned().unwrap_or(ToolPolicy {
            exposed: true,
            mutates: None,
            latency: None,
            confirm: None,
        })
    }

    /// Whether a tool should be offered to the model at all.
    ///
    /// Unlisted means yes: a server just connected has no rows yet, and hiding
    /// everything until someone ticks a box would make a new server look
    /// broken.
    pub fn exposes(&self, tool: &str) -> bool {
        self.tools.get(tool).is_none_or(|p| p.exposed)
    }
}

pub fn servers() -> Result<Vec<Server>> {
    if !exists() {
        return Ok(Vec::new());
    }
    let conn = open()?;

    let mut q = conn.prepare(
        "SELECT name, transport, command, args, url, headers, enabled
         FROM mcp_server ORDER BY name",
    )?;
    let mut out: Vec<Server> = q
        .query_map([], |r| {
            let args: String = r.get(3)?;
            let headers: String = r.get(5)?;
            Ok(Server {
                name: r.get(0)?,
                transport: r.get(1)?,
                command: r.get(2)?,
                // Malformed JSON in a hand-edited row is an empty list, not a
                // failure to start. The row is still usable without it.
                args: serde_json::from_str(&args).unwrap_or_default(),
                url: r.get(4)?,
                headers: serde_json::from_str(&headers).unwrap_or_default(),
                enabled: r.get::<_, i64>(6)? != 0,
                tools: BTreeMap::new(),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut q = conn.prepare(
        "SELECT server, tool, exposed, mutates, latency, confirm FROM mcp_tool",
    )?;
    let rows = q.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            ToolPolicy {
                exposed: r.get::<_, i64>(2)? != 0,
                mutates: r.get::<_, Option<i64>>(3)?.map(|v| v != 0),
                latency: r.get(4)?,
                confirm: r.get(5)?,
            },
        ))
    })?;
    for row in rows {
        let (server, tool, policy) = row?;
        if let Some(s) = out.iter_mut().find(|s| s.name == server) {
            s.tools.insert(tool, policy);
        }
    }
    Ok(out)
}

pub fn server_get(name: &str) -> Result<Option<Server>> {
    Ok(servers()?.into_iter().find(|s| s.name == name))
}

/// Writes one server, replacing any row of the same name.
///
/// Does not touch `mcp_tool`: editing a server's URL must not silently discard
/// the judgements someone made about its tools.
pub fn server_set(s: &Server) -> Result<()> {
    open()?
        .execute(
            "INSERT INTO mcp_server (name, transport, command, args, url, headers, enabled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(name) DO UPDATE SET
                 transport = excluded.transport,
                 command   = excluded.command,
                 args      = excluded.args,
                 url       = excluded.url,
                 headers   = excluded.headers,
                 enabled   = excluded.enabled",
            (
                &s.name,
                &s.transport,
                &s.command,
                serde_json::to_string(&s.args).unwrap_or_else(|_| "[]".into()),
                &s.url,
                serde_json::to_string(&s.headers).unwrap_or_else(|_| "{}".into()),
                i64::from(s.enabled),
            ),
        )
        .with_context(|| format!("save server {}", s.name))?;
    Ok(())
}

/// Removes a server and every judgement made about its tools. Deliberately
/// both: a server that is gone has no tools to have opinions about, and leaving
/// orphan rows would silently re-apply them if the name were ever reused.
pub fn server_delete(name: &str) -> Result<()> {
    let conn = open()?;
    conn.execute("DELETE FROM mcp_tool WHERE server = ?1", (name,))?;
    conn.execute("DELETE FROM mcp_env WHERE server = ?1", (name,))?;
    conn.execute("DELETE FROM mcp_oauth WHERE server = ?1", (name,))?;
    conn.execute("DELETE FROM mcp_server WHERE name = ?1", (name,))
        .with_context(|| format!("remove server {name}"))?;
    Ok(())
}

pub fn tool_policy_set(server: &str, tool: &str, p: &ToolPolicy) -> Result<()> {
    open()?
        .execute(
            "INSERT INTO mcp_tool (server, tool, exposed, mutates, latency, confirm)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(server, tool) DO UPDATE SET
                 exposed = excluded.exposed,
                 mutates = excluded.mutates,
                 latency = excluded.latency,
                 confirm = excluded.confirm",
            (
                server,
                tool,
                i64::from(p.exposed),
                p.mutates.map(i64::from),
                &p.latency,
                &p.confirm,
            ),
        )
        .with_context(|| format!("save policy for {server}/{tool}"))?;
    Ok(())
}

// ---------------------------------------------------- per-server environment --

/// The variables a server is given, by name. Values are in the keyring.
pub fn env_names(server: &str) -> Result<Vec<String>> {
    if !exists() {
        return Ok(Vec::new());
    }
    let conn = open()?;
    let mut q = conn.prepare("SELECT name FROM mcp_env WHERE server = ?1 ORDER BY name")?;
    let rows = q.query_map((server,), |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn env_add(server: &str, name: &str) -> Result<()> {
    open()?
        .execute(
            "INSERT INTO mcp_env (server, name) VALUES (?1, ?2)
             ON CONFLICT(server, name) DO NOTHING",
            (server, name),
        )
        .with_context(|| format!("record {name} for {server}"))?;
    Ok(())
}

pub fn env_delete(server: &str, name: &str) -> Result<()> {
    open()?
        .execute(
            "DELETE FROM mcp_env WHERE server = ?1 AND name = ?2",
            (server, name),
        )
        .with_context(|| format!("forget {name} for {server}"))?;
    Ok(())
}

// -------------------------------------------------------------------- oauth --

/// What is remembered about a server that signs in. Tokens are not here.
#[derive(Debug, Clone, Default)]
pub struct OAuth {
    pub client_id: Option<String>,
    pub scopes: Vec<String>,
}

pub fn oauth_get(server: &str) -> Result<Option<OAuth>> {
    if !exists() {
        return Ok(None);
    }
    let conn = open()?;
    let mut q = conn.prepare("SELECT client_id, scopes FROM mcp_oauth WHERE server = ?1")?;
    let mut rows = q.query_map((server,), |r| {
        let scopes: String = r.get(1)?;
        Ok(OAuth {
            client_id: r.get(0)?,
            scopes: serde_json::from_str(&scopes).unwrap_or_default(),
        })
    })?;
    Ok(rows.next().transpose()?)
}

pub fn oauth_set(server: &str, o: &OAuth) -> Result<()> {
    open()?
        .execute(
            "INSERT INTO mcp_oauth (server, client_id, scopes) VALUES (?1, ?2, ?3)
             ON CONFLICT(server) DO UPDATE SET
                 client_id = excluded.client_id,
                 scopes    = excluded.scopes",
            (
                server,
                &o.client_id,
                serde_json::to_string(&o.scopes).unwrap_or_else(|_| "[]".into()),
            ),
        )
        .with_context(|| format!("save sign-in for {server}"))?;
    Ok(())
}

pub fn oauth_delete(server: &str) -> Result<()> {
    open()?.execute("DELETE FROM mcp_oauth WHERE server = ?1", (server,))?;
    Ok(())
}

// ------------------------------------------------------------------- skills --

/// The index over `skills/*.md`. The body is not here -- it is in the file.
#[derive(Debug, Clone)]
pub struct SkillRow {
    pub name: String,
    pub path: String,
    pub description: String,
    pub enabled: bool,
}

pub fn skills() -> Result<Vec<SkillRow>> {
    if !exists() {
        return Ok(Vec::new());
    }
    let conn = open()?;
    let mut q =
        conn.prepare("SELECT name, path, description, enabled FROM skill ORDER BY name")?;
    let rows = q.query_map([], |r| {
        Ok(SkillRow {
            name: r.get(0)?,
            path: r.get(1)?,
            description: r.get(2)?,
            enabled: r.get::<_, i64>(3)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Records a skill found on disk, **keeping whatever `enabled` it already had**.
///
/// The scan runs at every start-up, so an upsert that wrote `enabled` would
/// turn every skill back on every time IRA restarted -- the switch would look
/// like it did nothing, which is worse than not having one.
pub fn skill_seen(name: &str, path: &str, description: &str) -> Result<()> {
    open()?
        .execute(
            "INSERT INTO skill (name, path, description, enabled) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(name) DO UPDATE SET
                 path = excluded.path,
                 description = excluded.description",
            (name, path, description),
        )
        .with_context(|| format!("record skill {name}"))?;
    Ok(())
}

pub fn skill_enable(name: &str, on: bool) -> Result<()> {
    let n = open()?
        .execute(
            "UPDATE skill SET enabled = ?2 WHERE name = ?1",
            (name, i64::from(on)),
        )
        .with_context(|| format!("update skill {name}"))?;
    if n == 0 {
        anyhow::bail!("no such skill: {name}");
    }
    Ok(())
}

/// Forgets skills whose files are gone, so a deleted `.md` does not leave a row
/// the window would still list.
pub fn skills_prune(keep: &[String]) -> Result<()> {
    let conn = open()?;
    let mut stale = Vec::new();
    {
        let mut q = conn.prepare("SELECT name FROM skill")?;
        let rows = q.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            let name = row?;
            if !keep.contains(&name) {
                stale.push(name);
            }
        }
    }
    for name in &stale {
        conn.execute("DELETE FROM skill WHERE name = ?1", (name.as_str(),))?;
    }
    if !stale.is_empty() {
        tracing::info!(gone = ?stale, "skills removed from the index");
    }
    Ok(())
}

/// The working directory is process-wide, and `PATH` is relative to it, so a
/// test that moves it moves it for every other test running at that moment.
/// Every test that does so takes this first.
///
/// Not a niceness: two tests each chdir-ing to their own temp directory
/// produced a failure that looked like a lost database row, which is a long way
/// from the actual cause.
#[cfg(test)]
pub fn cwd_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test here writes the one database in the working directory, so
    /// they share a temp directory and a lock rather than racing each other.
    fn in_a_fresh_db<T>(f: impl FnOnce() -> T) -> T {
        let _guard = crate::db::cwd_lock();

        let dir = std::env::temp_dir().join("ira-db-test");
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let _ = std::fs::remove_file(PATH);

        let out = f();
        std::env::set_current_dir(cwd).unwrap();
        out
    }

    /// A server survives the round trip, and editing it does not discard the
    /// judgements made about its tools -- which would silently re-arm the
    /// confirmation gate on tools someone had already vouched for.
    #[test]
    fn editing_a_server_keeps_its_tool_policy() {
        in_a_fresh_db(|| {
            let mut s = Server {
                name: "calendar".into(),
                transport: "stdio".into(),
                command: Some("mcp-calendar".into()),
                args: vec!["--fast".into()],
                enabled: true,
                ..Default::default()
            };
            server_set(&s).unwrap();
            tool_policy_set(
                "calendar",
                "list_events",
                &ToolPolicy { exposed: true, mutates: Some(false), ..Default::default() },
            )
            .unwrap();

            s.command = Some("mcp-calendar-2".into());
            server_set(&s).unwrap();

            let back = server_get("calendar").unwrap().unwrap();
            assert_eq!(back.command.as_deref(), Some("mcp-calendar-2"));
            assert_eq!(back.args, vec!["--fast".to_string()]);
            assert_eq!(back.policy("list_events").mutates, Some(false));
            // The dangerous default: anything nobody judged still asks first.
            assert_eq!(back.policy("delete_everything").mutates, None);
            assert!(back.exposes("delete_everything"));
        });
    }

    /// Deleting a server takes its tool rows with it. Orphans would re-apply
    /// silently if the name were ever reused.
    #[test]
    fn deleting_a_server_takes_its_judgements_with_it() {
        in_a_fresh_db(|| {
            let s = Server {
                name: "x".into(),
                transport: "http".into(),
                url: Some("http://127.0.0.1:1".into()),
                enabled: true,
                ..Default::default()
            };
            server_set(&s).unwrap();
            tool_policy_set(
                "x",
                "send",
                &ToolPolicy { exposed: true, mutates: Some(false), ..Default::default() },
            )
            .unwrap();
            server_delete("x").unwrap();

            server_set(&s).unwrap();
            let back = server_get("x").unwrap().unwrap();
            assert_eq!(
                back.policy("send").mutates,
                None,
                "a reused name must not inherit the old judgement"
            );
        });
    }

    /// The scan runs at every start-up. If it wrote `enabled`, every restart
    /// would turn every skill back on and the switch would look broken.
    #[test]
    fn rescanning_does_not_re_enable_a_skill_that_was_turned_off() {
        in_a_fresh_db(|| {
            skill_seen("standup", "skills/standup.md", "A standup.").unwrap();
            skill_enable("standup", false).unwrap();
            skill_seen("standup", "skills/standup.md", "A standup, edited.").unwrap();

            let rows = skills().unwrap();
            assert_eq!(rows.len(), 1);
            assert!(!rows[0].enabled, "a rescan must not turn it back on");
            assert_eq!(rows[0].description, "A standup, edited.");

            // A file that is gone leaves no row behind.
            skills_prune(&[]).unwrap();
            assert!(skills().unwrap().is_empty());
        });
    }
}
