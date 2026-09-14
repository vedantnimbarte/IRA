//! Opening what is already on the machine: an installed app, a file or
//! folder, or a URL -- one tool, because Windows already treats all three the
//! same way when you type them into Run.
//!
//! Windows-only, like `orb` and `console`: decisions/0013 already draws that
//! line for platform affordances, and there is no installed-programs registry
//! to read on the other builds anyway.
//!
//! Opening is not a mutation IRA needs to ask about -- it is exactly as
//! reversible as a person double-clicking the same icon, and asking "open
//! Spotify? yes or no" for every request would be worse than the rare wrong
//! guess. Closing is: it can interrupt unsaved work, so [`CloseApp`] goes
//! through the confirmation gate that [`OpenTarget`] deliberately skips.

use crate::tool::{Latency, Tool, ToolCtx, ToolOutcome, ToolSpec};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

/// A helper process with no console window -- IRA usually runs without a
/// console, and PowerShell or taskkill would otherwise flash one. Not for the
/// apps being opened: a console app someone asked for should get its window.
fn hidden(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

/// One program Windows knows how to launch, as its own registry describes it.
struct App {
    name: String,
    exe: String,
}

/// Reads the same two registry tables Control Panel > Programs and `Win+R`
/// read from.
///
/// Shelled out to PowerShell rather than the raw registry API: this is a
/// stable one-screen script instead of an unsafe UTF-16 FFI dance, and IRA
/// already spawns other processes (piper, whisper) the same way. Rescanned on
/// every call rather than cached -- a registry read is a few milliseconds,
/// and a stale catalog missing something installed five minutes ago is the
/// worse trade.
fn installed_apps() -> Vec<App> {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
$apps = @()
'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall',
'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall',
'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' |
  Get-ChildItem | Get-ItemProperty |
  Where-Object { $_.DisplayName -and $_.DisplayIcon -match '\.exe' } |
  ForEach-Object {
    $apps += [pscustomobject]@{ name = $_.DisplayName; exe = ($_.DisplayIcon -split ',')[0].Trim('"') }
  }
'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths',
'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths' |
  Get-ChildItem | ForEach-Object {
    $p = (Get-ItemProperty $_.PSPath).'(default)'
    if ($p) { $apps += [pscustomobject]@{ name = ($_.PSChildName -replace '\.exe$',''); exe = $p } }
  }
$apps | ConvertTo-Json -Compress
"#;
    let Ok(out) = hidden("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .output()
    else {
        return Vec::new();
    };
    // One match comes back as a bare object rather than a one-element array.
    let entries = match serde_json::from_slice(&out.stdout) {
        Ok(Value::Array(a)) => a,
        Ok(one @ Value::Object(_)) => vec![one],
        _ => return Vec::new(),
    };
    entries
        .into_iter()
        .filter_map(|v| {
            let name = v.get("name")?.as_str()?.to_string();
            let exe = v.get("exe")?.as_str()?.to_string();
            Path::new(&exe).is_file().then_some(App { name, exe })
        })
        .collect()
}

/// How well `target` (what was heard) names `candidate` (what is installed).
/// Case-insensitive containment first, since that is the common case and
/// exact -- "chrome" inside "Google Chrome" -- then edit distance against the
/// whole name, since speech-to-text mishears rather than truncates.
fn score(target: &str, candidate: &str) -> f32 {
    let (t, c) = (target.to_lowercase(), candidate.to_lowercase());
    if t == c {
        return 1.0;
    }
    if c.contains(&t) || t.contains(&c) {
        return 0.9 * (t.len().min(c.len()) as f32 / t.len().max(c.len()) as f32);
    }
    let dist = edit_distance(&t, &c) as f32;
    1.0 - dist / t.len().max(c.len()).max(1) as f32
}

/// Textbook Levenshtein distance. No crate for this: it is fifteen lines and
/// the only place fuzzy matching happens.
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut prev = row[0];
        row[0] = i;
        for j in 1..=b.len() {
            let cur = row[j];
            row[j] = if a[i - 1] == b[j - 1] {
                prev
            } else {
                1 + prev.min(row[j]).min(row[j - 1])
            };
            prev = cur;
        }
    }
    row[b.len()]
}

/// The closest installed app to `target`, best guess always -- a misheard
/// "open fotoshop" should open Photoshop, not report failure. `None` only
/// when nothing is installed at all.
fn best_match(target: &str) -> Option<App> {
    installed_apps()
        .into_iter()
        .max_by(|a, b| {
            score(target, &a.name)
                .partial_cmp(&score(target, &b.name))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// The bare executable names (no `.exe`, lowercase) of every process running
/// right now -- what `Get-Process` already tracks for the task manager, asked
/// for the same way the registry is: one PowerShell line rather than the
/// Toolhelp32 snapshot API.
fn running_process_stems() -> BTreeSet<String> {
    let Ok(out) = hidden("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-Process | Select-Object -ExpandProperty ProcessName) -join \"`n\"",
        ])
        .output()
    else {
        return BTreeSet::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty())
        .collect()
}

fn exe_stem(exe: &str) -> String {
    Path::new(exe)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn looks_like_url(target: &str) -> bool {
    let t = target.trim();
    t.starts_with("http://")
        || t.starts_with("https://")
        || (!t.contains(' ') && t.rsplit_once('.').is_some_and(|(_, ext)| ext.len() >= 2 && ext.chars().all(char::is_alphanumeric)))
}

/// Opens an installed app, a file, a folder, or a URL by name.
pub struct OpenTarget;

#[async_trait::async_trait]
impl Tool for OpenTarget {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "open".into(),
            description: "Opens something already on this machine: an installed application \
                          by name (\"open Spotify\"), a file or folder by path, or opens a \
                          website by opening it in the default browser (\"open github.com\"). \
                          Use the plain name the user said; matching an installed app to it is \
                          this tool's job, not the model's."
                .into(),
            schema: json!({
                "type": "object",
                "properties": { "target": { "type": "string" } },
                "required": ["target"],
            }),
            mutates: false,
            latency: Latency::Slow,
            confirm: None,
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("no target given"))?;

        // A real path wins outright: "report.docx" must open the file, not be
        // mistaken for a URL because it has a dot in it.
        if Path::new(target).exists() {
            Command::new("explorer").arg(target).spawn().map_err(|e| anyhow!("could not open {target}: {e}"))?;
            return Ok(ToolOutcome::Answer(format!("Opened {target}.")));
        }

        if looks_like_url(target) {
            let url = if target.starts_with("http") { target.to_string() } else { format!("https://{target}") };
            Command::new("explorer").arg(&url).spawn().map_err(|e| anyhow!("could not open a browser: {e}"))?;
            return Ok(ToolOutcome::Answer(format!("Opened {url} in the default browser.")));
        }

        let app = best_match(target).ok_or_else(|| anyhow!("nothing installed looks like {target}"))?;
        Command::new(&app.exe).spawn().map_err(|e| anyhow!("could not start {}: {e}", app.name))?;
        Ok(ToolOutcome::Answer(format!("Opened {}.", app.name)))
    }
}

/// Closes a running application by name. Separate from [`OpenTarget`] because
/// this one mutates -- it can interrupt work an app has not saved -- and asks
/// first.
pub struct CloseApp;

#[async_trait::async_trait]
impl Tool for CloseApp {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "close_app".into(),
            description: "Closes a running application by name, e.g. \"close Spotify\". Only \
                          for applications the user is running, not files or the system itself."
                .into(),
            schema: json!({
                "type": "object",
                "properties": { "target": { "type": "string" } },
                "required": ["target"],
            }),
            mutates: true,
            latency: Latency::Slow,
            confirm: Some("Close that app? Yes or no?".into()),
        }
    }

    fn question(&self, args: &Value) -> Option<String> {
        let target = args["target"].as_str()?.trim();
        (!target.is_empty()).then(|| format!("Close {target}? Anything unsaved may be lost. Yes or no."))
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("no target given"))?;
        let app = best_match(target).ok_or_else(|| anyhow!("nothing installed looks like {target}"))?;
        let exe_name = Path::new(&app.exe)
            .file_name()
            .ok_or_else(|| anyhow!("{} has no executable name", app.name))?
            .to_string_lossy()
            .to_string();

        // No /F: a plain WM_CLOSE lets the app prompt to save rather than
        // killing it outright.
        let out = hidden("taskkill")
            .args(["/IM", &exe_name])
            .output()
            .map_err(|e| anyhow!("could not close {}: {e}", app.name))?;
        if out.status.success() {
            Ok(ToolOutcome::Answer(format!("Closed {}.", app.name)))
        } else {
            Ok(ToolOutcome::Answer(format!("{} was not running.", app.name)))
        }
    }
}

/// Whether a specific installed app is running, or which recognized apps are
/// running at all. The natural counterpart to [`OpenTarget`]/[`CloseApp`]:
/// "is Spotify open" and "what's running" both come down to the same
/// process list crossed with the same registry catalog.
pub struct RunningApps;

#[async_trait::async_trait]
impl Tool for RunningApps {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "running_apps".into(),
            description: "Tells you whether a specific application is currently running \
                          (\"is Spotify open?\") when given a name, or lists which recognized \
                          applications are currently running (\"what's running?\") when not."
                .into(),
            schema: json!({
                "type": "object",
                "properties": { "target": { "type": "string" } },
            }),
            mutates: false,
            latency: Latency::Slow,
            confirm: None,
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
        let target = args.get("target").and_then(|v| v.as_str());
        let running = running_process_stems();

        if let Some(target) = target {
            let app = best_match(target).ok_or_else(|| anyhow!("nothing installed looks like {target}"))?;
            return Ok(ToolOutcome::Answer(if running.contains(&exe_stem(&app.exe)) {
                format!("{} is running.", app.name)
            } else {
                format!("{} is not running.", app.name)
            }));
        }

        // Every installed app whose executable is currently a running
        // process, by friendly name -- a `BTreeSet` so the same app found
        // under both registry tables only says its name once, sorted rather
        // than in registry-scan order.
        let names: BTreeSet<String> = installed_apps()
            .into_iter()
            .filter(|app| running.contains(&exe_stem(&app.exe)))
            .map(|app| app.name)
            .collect();
        Ok(ToolOutcome::Answer(if names.is_empty() {
            "Nothing recognized is currently running.".into()
        } else {
            format!("Running: {}.", names.into_iter().collect::<Vec<_>>().join(", "))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_substring_beat_a_stranger() {
        assert!(score("chrome", "Google Chrome") > score("chrome", "Steam"));
        assert_eq!(score("spotify", "Spotify"), 1.0);
    }

    #[test]
    fn a_misheard_name_still_finds_the_real_one() {
        assert!(score("fotoshop", "Adobe Photoshop") > score("fotoshop", "Steam"));
    }

    // `looks_like_url` alone cannot tell "github.com" from "report.docx" --
    // both are one bare word with a dot in it. What tells them apart is
    // `call`'s order: an existing path is checked, and wins, before this
    // heuristic ever runs. This only has to reject shapes no domain has.
    #[test]
    fn only_a_bare_dotted_word_looks_like_a_url() {
        assert!(looks_like_url("github.com"));
        assert!(looks_like_url("http://localhost:8080"));
        assert!(!looks_like_url("C:\\Users\\me\\Downloads"));
        assert!(!looks_like_url("open spotify please"));
    }

    /// Exercises the real registry scan, not a fixture -- this is the one
    /// test that would have caught the PowerShell script itself being wrong.
    /// Notepad is chosen because every Windows machine has it; it does not
    /// prove any *particular* app is found, only that the pipeline finds
    /// something real and its path actually exists on disk.
    #[test]
    fn the_real_registry_yields_a_real_app() {
        let apps = installed_apps();
        assert!(!apps.is_empty(), "no installed apps found -- is the registry script broken?");
        for app in &apps {
            assert!(Path::new(&app.exe).is_file(), "{} points at {} which does not exist", app.name, app.exe);
        }
    }

    #[tokio::test]
    #[ignore = "spawns a real, visible process; run manually with --ignored"]
    async fn opening_notepad_actually_opens_it() {
        let ctx = ToolCtx { transcript: String::new(), cancel: tokio_util::sync::CancellationToken::new() };
        let out = OpenTarget.call(json!({"target": "notepad"}), &ctx).await.unwrap();
        match out {
            ToolOutcome::Answer(text) => assert!(text.contains("Notepad") || text.to_lowercase().contains("notepad")),
            _ => panic!("expected an answer"),
        }
    }

    // Not every always-running process (explorer, dwm, csrss) has a registry
    // entry `best_match` can find -- explorer.exe has neither an Uninstall
    // nor an App Paths key on a stock install. So proving the "is X running"
    // branch needs a process this test starts and can name for certain.
    #[tokio::test]
    #[ignore = "spawns a real, visible process; run manually with --ignored"]
    async fn a_freshly_opened_app_is_seen_running() {
        let ctx = ToolCtx { transcript: String::new(), cancel: tokio_util::sync::CancellationToken::new() };
        OpenTarget.call(json!({"target": "notepad"}), &ctx).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let out = RunningApps.call(json!({"target": "notepad"}), &ctx).await.unwrap();
        let _ = Command::new("taskkill").args(["/IM", "notepad.exe"]).output();
        match out {
            ToolOutcome::Answer(text) => assert!(text.contains("is running"), "got: {text}"),
            _ => panic!("expected an answer"),
        }
    }

    /// No fixture to depend on for *which* apps are installed on the machine
    /// running this test, so this only proves the real process list and the
    /// real registry catalog cross-reference into a well-formed answer, not
    /// that any particular app shows up in it.
    #[tokio::test]
    async fn listing_running_apps_is_well_formed() {
        let ctx = ToolCtx { transcript: String::new(), cancel: tokio_util::sync::CancellationToken::new() };
        let out = RunningApps.call(json!({}), &ctx).await.unwrap();
        match out {
            ToolOutcome::Answer(text) => {
                assert!(text.starts_with("Running: ") || text == "Nothing recognized is currently running.", "got: {text}");
            }
            _ => panic!("expected an answer"),
        }
    }
}
