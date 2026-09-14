//! What was asked, and what was done about it.
//!
//! The transcript records what was *said*. This records what *happened*: every
//! tool and skill a turn used, with its arguments and result; every yes/no
//! question and its answer; every change made in the settings window; every
//! reminder that went off. The settings window shows it as Activity, and the
//! `recall` tool lets IRA look back through it.
//!
//! **A listener, not a hook in every path.** Nearly everything already passes
//! through `Ui::send` on its way to the screen, so the recorder subscribes to
//! that broadcast on its own thread and turns events into rows. Nothing in the
//! loop, the tool registry or the model code knows it exists, and a slow disk
//! cannot hold up a turn. The few things that never reach the screen -- a
//! settings change, a reminder, words from `POST /say` -- call [`record`]
//! where they happen.
//!
//! **Rows share a `grp`.** Every event of one turn carries the same group, so a
//! search that matches a tool's arguments can bring back what was said around
//! it. A standalone action is a group of one.
//!
//! **Secrets are masked, long results are cut.** Arguments and results are
//! kept, because a log that only says "a tool ran" answers nothing -- but
//! anything shaped like a key or token is replaced, and a result longer than a
//! few KB is truncated. `IRA_AUDIT=off` records nothing at all.

use crate::db::{self, AuditEntry, AuditQuery, AuditRow};
use crate::tool::{Latency, Tool, ToolCtx, ToolOutcome, ToolSpec};
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

/// Longest argument or result kept, in bytes.
const CLIP: usize = 4 * 1024;
const DAY_MS: i64 = 86_400_000;
pub const DEFAULT_RETENTION_DAYS: i64 = 90;
const RETENTION_KEY: &str = "audit_retention_days";
/// How often the recorder drops groups past their retention.
const PRUNE_EVERY: Duration = Duration::from_secs(6 * 3600);

/// `IRA_AUDIT=off` turns recording off. The log that is already there stays
/// readable.
pub fn enabled() -> bool {
    std::env::var("IRA_AUDIT").map(|v| v != "off").unwrap_or(true)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A new group key: the current time in milliseconds, nudged forward if two
/// groups start in the same millisecond. Time-shaped so groups sort by when
/// they began and pruning by age is a comparison on the key.
fn next_grp() -> i64 {
    static LAST: AtomicI64 = AtomicI64::new(0);
    let now = now_ms();
    let prev = LAST
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |last| Some(now.max(last + 1)))
        .unwrap_or_else(|p| p);
    now.max(prev + 1)
}

/// Days to keep, or 0 for forever.
pub fn retention_days() -> i64 {
    db::settings_all()
        .ok()
        .and_then(|s| s.get(RETENTION_KEY).and_then(|v| v.parse().ok()))
        .unwrap_or(DEFAULT_RETENTION_DAYS)
}

pub fn set_retention_days(days: i64) -> Result<()> {
    anyhow::ensure!((0..=3650).contains(&days), "keep it between a day and ten years, or forever");
    db::settings_set(RETENTION_KEY, &days.to_string())?;
    prune();
    Ok(())
}

fn prune() {
    let days = retention_days();
    if days == 0 {
        return;
    }
    match db::audit_prune(now_ms() - days * DAY_MS) {
        Ok(0) => {}
        Ok(n) => tracing::info!(rows = n, days, "activity pruned"),
        Err(e) => tracing::error!(?e, "activity prune failed"),
    }
}

// ---------------------------------------------------------------- redaction --

/// Prefixes of credentials that announce themselves: Anthropic and OpenAI
/// (`sk-`), Groq, GitHub, GitLab, Slack, AWS, Google, and JWTs.
const SECRET_PREFIXES: &[&str] = &[
    "sk-", "gsk_", "ghp_", "gho_", "ghs_", "ghu_", "github_pat_", "glpat-", "xoxb-", "xoxp-",
    "AKIA", "AIza", "eyJ",
];

/// A JSON key whose value is a credential whatever it looks like.
fn secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k == "key"
        || ["token", "secret", "password", "passwd", "authorization", "cookie", "credential", "api_key", "apikey"]
            .iter()
            .any(|w| k.contains(w))
}

/// Masks anything shaped like a credential.
///
/// ponytail: shape-based, so a key with no recognisable prefix and no
/// `Bearer` in front of it gets through. Masking the exact values held in
/// the keyring would close that, at a keyring read per event.
pub fn redact(text: &str) -> String {
    fn flush(run: &mut String, out: &mut String, bearer: &mut bool) {
        if run.is_empty() {
            return;
        }
        let secret = (*bearer && run.len() >= 16)
            || (run.len() >= 20 && SECRET_PREFIXES.iter().any(|p| run.starts_with(p)));
        out.push_str(if secret { "[redacted]" } else { run });
        *bearer = run.eq_ignore_ascii_case("bearer");
        run.clear();
    }
    let (mut out, mut run, mut bearer) = (String::with_capacity(text.len()), String::new(), false);
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
            run.push(c);
        } else {
            flush(&mut run, &mut out, &mut bearer);
            out.push(c);
        }
    }
    flush(&mut run, &mut out, &mut bearer);
    out
}

fn redact_json(v: Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let v = if secret_key(&k) && !v.is_null() { json!("[redacted]") } else { redact_json(v) };
                    (k, v)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(redact_json).collect()),
        Value::String(s) => Value::String(redact(&s)),
        other => other,
    }
}

/// Tool arguments arrive as JSON text. Redacted structurally when they parse,
/// and as plain text when they do not.
fn redact_args(args: &str) -> String {
    match serde_json::from_str::<Value>(args) {
        Ok(v) => clip(&redact_json(v).to_string()),
        Err(_) => clip(&redact(args)),
    }
}

fn clip(text: &str) -> String {
    if text.len() <= CLIP {
        return text.to_string();
    }
    let mut end = CLIP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [{} more bytes not kept]", &text[..end], text.len() - end)
}

fn clean(text: &str) -> String {
    clip(&redact(text))
}

// ---------------------------------------------------------------- recording --

/// A standalone action -- a settings change, a reminder going off, words
/// queued from outside. A group of its own. Never fails the caller: this is a
/// record of the work, not the work.
pub fn record(kind: &str, name: &str, detail: &str, result: Option<&str>, ok: Option<bool>) {
    if !enabled() {
        return;
    }
    let (detail, result) = (clean(detail), result.map(clean));
    let entry = AuditEntry {
        grp: next_grp(),
        at: now_ms(),
        kind,
        name: Some(name),
        detail: Some(&detail),
        result: result.as_deref(),
        ok,
        ms: None,
    };
    if let Err(e) = db::audit_insert(&entry) {
        tracing::error!(?e, kind, "activity not recorded");
    }
}

/// A tool run by hand with the settings window's "Try it", which calls the
/// tool directly and so never reaches the event stream.
pub fn record_try(tool: &str, args: &str, outcome: &Result<String>) {
    if !enabled() {
        return;
    }
    let (result, ok) = match outcome {
        Ok(text) => (clean(text), true),
        Err(e) => (clean(&format!("{e:#}")), false),
    };
    let args = redact_args(args);
    let entry = AuditEntry {
        grp: next_grp(),
        at: now_ms(),
        kind: "tool_try",
        name: Some(tool),
        detail: Some(&args),
        result: Some(&result),
        ok: Some(ok),
        ms: None,
    };
    if let Err(e) = db::audit_insert(&entry) {
        tracing::error!(?e, "activity not recorded");
    }
}

/// Turns the screen's event stream into rows, tracking which turn each event
/// belongs to.
#[derive(Default)]
struct Recorder {
    /// The turn in progress. Set by `heard`, cleared by the `turn` line that
    /// ends it.
    grp: Option<i64>,
    /// That turn's `heard` row, which the `turn` line gives a duration.
    heard: Option<i64>,
    /// Tool rows still waiting on a result: (row, tool, started). Not tied to
    /// the turn, because a background job reports long after it ends.
    tools: Vec<(i64, String, i64)>,
    /// The yes/no question waiting on an answer.
    confirm: Option<i64>,
}

/// A barged-in call can be dropped without ever reporting, so waiting rows
/// are capped rather than left to pile up.
const PENDING_MAX: usize = 32;

impl Recorder {
    fn insert(&self, grp: i64, kind: &str, name: Option<&str>, detail: Option<&str>) -> Result<i64> {
        db::audit_insert(&AuditEntry { grp, at: now_ms(), kind, name, detail, ..Default::default() })
    }

    /// The turn in progress, or a group of one for an event outside any turn
    /// -- a confirmation the settings window asked for, a failure before
    /// anything was heard.
    fn grp(&self) -> i64 {
        self.grp.unwrap_or_else(next_grp)
    }

    fn handle(&mut self, v: &Value) -> Result<()> {
        let s = |k: &str| v[k].as_str().unwrap_or_default();
        match s("kind") {
            "heard" => {
                let grp = next_grp();
                self.grp = Some(grp);
                self.confirm = None;
                self.heard = Some(self.insert(grp, "heard", None, Some(&clean(s("text"))))?);
            }
            "tool" => {
                let id = self.insert(self.grp(), "tool", Some(s("name")), Some(&redact_args(s("args"))))?;
                if self.tools.len() == PENDING_MAX {
                    self.tools.remove(0);
                }
                self.tools.push((id, s("name").to_string(), now_ms()));
            }
            "result" => {
                let ok = v["ok"].as_bool();
                let text = clean(s("text"));
                match self.tools.iter().rposition(|(_, name, _)| name == s("name")) {
                    Some(i) => {
                        let (id, _, started) = self.tools.remove(i);
                        db::audit_finish(id, Some(&text), ok, Some(now_ms() - started))?;
                    }
                    None => {
                        let id = self.insert(self.grp(), "tool", Some(s("name")), None)?;
                        db::audit_finish(id, Some(&text), ok, None)?;
                    }
                }
            }
            "confirm" => {
                self.confirm = Some(self.insert(self.grp(), "confirm", None, Some(&clean(s("question"))))?);
            }
            "answered" => {
                if let Some(id) = self.confirm.take() {
                    let yes = v["yes"].as_bool().unwrap_or(false);
                    db::audit_finish(id, Some(if yes { "yes" } else { "no" }), Some(yes), None)?;
                }
            }
            // Outside a turn a reply is a report or a reminder being read out,
            // and those are recorded where they come from.
            "reply" => {
                if let Some(grp) = self.grp {
                    self.insert(grp, "reply", None, Some(&clean(s("text"))))?;
                }
            }
            "failed" => {
                self.insert(self.grp(), "failed", None, Some(s("what")))?;
            }
            "turn" => {
                if let Some(id) = self.heard.take() {
                    db::audit_finish(id, None, None, v["total_ms"].as_i64())?;
                }
                self.grp = None;
                self.confirm = None;
            }
            _ => {}
        }
        Ok(())
    }
}

/// Starts recording. Call before anything is said, so the first turn is in it.
///
/// Its own thread rather than a task: every row is a blocking SQLite write,
/// and a thread with a blocking receiver keeps that entirely off the runtime.
pub fn start(ui: &crate::ui::Ui) {
    if !enabled() {
        tracing::info!("activity log off");
        return;
    }
    tracing::info!(path = %db::path().display(), days = retention_days(), "activity log");
    let mut rx = ui.subscribe();
    let spawned = std::thread::Builder::new().name("audit".into()).spawn(move || {
        use tokio::sync::broadcast::error::RecvError;
        prune();
        let mut pruned = Instant::now();
        let mut rec = Recorder::default();
        loop {
            match rx.blocking_recv() {
                Ok(json) => {
                    let Ok(v) = serde_json::from_str::<Value>(&json) else { continue };
                    if let Err(e) = rec.handle(&v) {
                        tracing::error!(?e, "activity not recorded");
                    }
                }
                // Fell behind the screen. What was missed is gone; say so.
                Err(RecvError::Lagged(n)) => tracing::warn!(missed = n, "activity log fell behind"),
                Err(RecvError::Closed) => break,
            }
            if pruned.elapsed() > PRUNE_EVERY {
                prune();
                pruned = Instant::now();
            }
        }
    });
    if let Err(e) = spawned {
        tracing::error!(?e, "activity log could not start");
    }
}

// ------------------------------------------------------------------ reading --

/// Groups of rows, in the shape the Activity pane draws.
fn grouped(rows: Vec<AuditRow>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for row in rows {
        if out.last().map(|g| g["grp"].as_i64() != Some(row.grp)).unwrap_or(true) {
            out.push(json!({ "grp": row.grp, "at": row.at, "rows": [] }));
        }
        if let Some(Value::Array(rows)) = out.last_mut().map(|g| &mut g["rows"]) {
            rows.push(json!(row));
        }
    }
    out
}

const PAGE: usize = 25;

/// One page of the Activity pane: `{q, filter, before}` in, groups out.
pub fn page(v: &Value) -> Value {
    let kinds: &[&str] = match v["filter"].as_str().unwrap_or_default() {
        "conversations" => &["heard"],
        "tools" => &["tool", "tool_try"],
        "confirmations" => &["confirm"],
        "settings" => &["setting"],
        "reminders" => &["reminder"],
        "outside" => &["say"],
        _ => &[],
    };
    let rows = db::audit_groups(&AuditQuery {
        text: v["q"].as_str().unwrap_or_default().trim(),
        kinds,
        before: v["before"].as_i64(),
        since: None,
        limit: PAGE,
    });
    match rows {
        Ok(rows) => {
            let groups = grouped(rows);
            json!({ "groups": groups, "more": groups.len() == PAGE })
        }
        Err(e) => json!({ "error": format!("{e:#}") }),
    }
}

/// "3 hours ago", in words a voice can say.
fn ago(ms: i64) -> String {
    let mins = ms / 60_000;
    match mins {
        m if m < 1 => "just now".into(),
        1 => "a minute ago".into(),
        m if m < 60 => format!("{m} minutes ago"),
        m if m < 120 => "an hour ago".into(),
        m if m < 24 * 60 => format!("{} hours ago", m / 60),
        m if m < 48 * 60 => "yesterday".into(),
        m => format!("{} days ago", m / (24 * 60)),
    }
}

fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!("{}…", text.chars().take(max).collect::<String>())
}

/// A group as a few plain lines for the model to answer from.
fn describe(group: &Value, now: i64) -> String {
    let mut lines = vec![ago(now - group["at"].as_i64().unwrap_or(now))];
    for row in group["rows"].as_array().into_iter().flatten() {
        let f = |k: &str| row[k].as_str().unwrap_or_default();
        let line = match f("kind") {
            "heard" => format!("  User said: {}", short(f("detail"), 200)),
            "reply" => format!("  IRA replied: {}", short(f("detail"), 200)),
            "tool" => format!(
                "  Used {} {} -> {}",
                f("name"),
                short(f("detail"), 120),
                if row["result"].is_null() { "no result".into() } else { short(f("result"), 160) }
            ),
            "confirm" => format!(
                "  Asked \"{}\" -> {}",
                f("detail"),
                if row["result"].is_null() { "no clear answer" } else { f("result") }
            ),
            "tool_try" => format!(
                "  Tried {} by hand in settings {} -> {}",
                f("name"),
                short(f("detail"), 120),
                short(f("result"), 160)
            ),
            "failed" => format!("  Failed: {}", f("detail")),
            "setting" => format!("  In settings: {}", f("detail")),
            "reminder" => format!("  Reminder went off: {}", short(f("detail"), 200)),
            "say" => format!("  Asked from outside to say: {}", short(f("detail"), 200)),
            _ => continue,
        };
        lines.push(line);
    }
    lines.join("\n")
}

/// Cap on what one recall hands the model.
const RECALL_MAX: usize = 3000;

/// Looks back through the activity log.
pub struct Recall;

#[async_trait::async_trait]
impl Tool for Recall {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "recall".into(),
            description: "Looks back through earlier conversations with IRA and what she did in them \
                          -- which tools and skills she used and what came of it. Use it when the user \
                          asks about something from before: 'what did I ask you about the flight', \
                          'what did you open earlier', 'what was that command you gave me yesterday'. \
                          `query` is a word or short phrase to find; leave it out for the most recent \
                          conversations. `days` is how far back to look (default 7)."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "days": { "type": "integer", "minimum": 1 },
                },
            }),
            mutates: false,
            latency: Latency::Slow,
            confirm: None,
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
        let query = args["query"].as_str().unwrap_or_default().trim();
        let days = args["days"].as_i64().unwrap_or(7).clamp(1, 3650);
        let now = now_ms();
        // The turn asking is already in the log, and matches its own question.
        // Only skipped if it is recent, so a log that missed it loses nothing.
        let before = db::audit_latest_grp("heard")?.filter(|g| now - g < 5 * 60_000);
        let rows = db::audit_groups(&AuditQuery {
            text: query,
            kinds: &[],
            before,
            since: Some(now - days * DAY_MS),
            limit: 8,
        })?;
        let groups = grouped(rows);
        if groups.is_empty() {
            let off = if enabled() { "" } else { " The activity log is turned off." };
            return Ok(ToolOutcome::Answer(if query.is_empty() {
                format!("Nothing in the activity log from the last {days} days.{off}")
            } else {
                format!("Nothing in the activity log from the last {days} days mentions \"{query}\".{off}")
            }));
        }
        let mut out = String::from("From the activity log, newest first:\n");
        for g in &groups {
            let text = describe(g, now);
            if out.len() + text.len() > RECALL_MAX {
                break;
            }
            out.push_str(&text);
            out.push('\n');
        }
        Ok(ToolOutcome::Answer(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_a_fresh_db<T>(f: impl FnOnce() -> T) -> T {
        let _guard = db::cwd_lock();
        let dir = std::env::temp_dir().join("ira-audit-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("IRA_DATA", &dir);
        let _ = std::fs::remove_file(db::path());
        let out = f();
        std::env::remove_var("IRA_DATA");
        out
    }

    #[test]
    fn keys_and_tokens_are_masked_and_words_are_not() {
        let out = redact("key sk-ant-api03-abcdefghijklmnopqrstuv and Bearer abcdefghijklmnopqrstu here");
        assert!(!out.contains("sk-ant"), "{out}");
        assert!(!out.contains("abcdefghijklmnopqrstu "), "{out}");
        assert!(out.starts_with("key [redacted] and Bearer [redacted] here"), "{out}");
        assert_eq!(redact("open spotify, then skype"), "open spotify, then skype");
    }

    #[test]
    fn secret_named_fields_are_masked_whatever_they_hold() {
        let out = redact_args(r#"{"target":"notepad","api_key":"hunter2","headers":{"Authorization":"x"}}"#);
        assert!(out.contains("notepad") && !out.contains("hunter2") && !out.contains("\"x\""), "{out}");
    }

    #[test]
    fn long_results_are_cut_on_a_character_boundary() {
        let text = "é".repeat(CLIP);
        let out = clip(&text);
        assert!(out.len() < text.len() && out.contains("more bytes not kept"));
    }

    #[test]
    fn group_keys_never_repeat() {
        let keys: Vec<i64> = (0..1000).map(|_| next_grp()).collect();
        assert!(keys.windows(2).all(|w| w[1] > w[0]));
    }

    /// The whole path: events as the screen receives them become one group,
    /// a result lands on its tool, and the answer lands on its question.
    #[test]
    fn a_turn_becomes_one_group_and_can_be_found() {
        in_a_fresh_db(|| {
            let mut rec = Recorder::default();
            for e in [
                json!({"kind":"heard","text":"close spotify"}),
                json!({"kind":"confirm","question":"Close spotify? Yes or no."}),
                json!({"kind":"answered","yes":true}),
                json!({"kind":"tool","name":"close_app","args":"{\"target\":\"spotify\"}"}),
                json!({"kind":"result","name":"close_app","ok":true,"text":"Closed Spotify."}),
                json!({"kind":"reply","text":"Done, Spotify is closed."}),
                json!({"kind":"turn","total_ms":1234}),
                // After the turn: a report read out is not a new turn.
                json!({"kind":"reply","text":"Your build finished."}),
            ] {
                rec.handle(&e).unwrap();
            }

            let groups = grouped(db::audit_groups(&AuditQuery { text: "spotify", limit: 10, ..Default::default() }).unwrap());
            assert_eq!(groups.len(), 1);
            let rows = groups[0]["rows"].as_array().unwrap();
            let kinds: Vec<&str> = rows.iter().map(|r| r["kind"].as_str().unwrap()).collect();
            assert_eq!(kinds, ["heard", "confirm", "tool", "reply"]);
            assert_eq!(rows[0]["ms"], 1234);
            assert_eq!(rows[1]["result"], "yes");
            assert_eq!(rows[2]["result"], "Closed Spotify.");
            assert_eq!(rows[2]["ok"], true);

            // A filter that the group has nothing of finds nothing.
            let none = db::audit_groups(&AuditQuery { kinds: &["setting"], limit: 10, ..Default::default() }).unwrap();
            assert!(none.is_empty());
        });
    }

    #[test]
    fn pruning_takes_whole_old_groups_only() {
        in_a_fresh_db(|| {
            let old = now_ms() - 100 * DAY_MS;
            db::audit_insert(&AuditEntry { grp: old, at: old, kind: "heard", detail: Some("old"), ..Default::default() }).unwrap();
            record("setting", "builtin_toggle", "Turned Open off", None, Some(true));
            prune();
            let left = db::audit_groups(&AuditQuery { limit: 10, ..Default::default() }).unwrap();
            assert_eq!(left.len(), 1);
            assert_eq!(left[0].kind, "setting");
        });
    }

    #[test]
    fn recall_skips_the_question_being_asked() {
        let out = in_a_fresh_db(|| {
            let mut rec = Recorder::default();
            for e in [
                json!({"kind":"heard","text":"open the flight booking site"}),
                json!({"kind":"tool","name":"open","args":"{\"target\":\"flights.example.com\"}"}),
                json!({"kind":"result","name":"open","ok":true,"text":"Opened https://flights.example.com."}),
                json!({"kind":"turn","total_ms":900}),
                json!({"kind":"heard","text":"what did I ask about the flight"}),
            ] {
                rec.handle(&e).unwrap();
            }
            let ctx = ToolCtx { transcript: String::new(), cancel: tokio_util::sync::CancellationToken::new() };
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(Recall.call(json!({"query":"flight"}), &ctx))
                .unwrap()
        });

        let ToolOutcome::Answer(text) = out else { panic!("expected an answer") };
        assert!(text.contains("open the flight booking site"), "{text}");
        assert!(text.contains("Opened https://flights.example.com."), "{text}");
        assert!(!text.contains("what did I ask about the flight"), "{text}");
    }
}
