//! Skills: instructions the user wrote, loaded on demand rather than always.
//!
//! A skill is one Markdown file in `skills/`. The filename is its name, an
//! optional `description:` in front matter is its one-line summary, and the
//! rest is text handed to the model when it asks for it.
//!
//! ```text
//! skills/standup.md
//!   ---
//!   description: How I write a standup update. Use when asked for one.
//!   ---
//!   Three lines: yesterday, today, blockers. Name people, not tickets.
//! ```
//!
//! **The file is the truth; the database is an index over it.** A row holds the
//! name, the path, the summary and whether the skill is on -- so the settings
//! window can list and switch skills without reading every file, and so a skill
//! stays a self-contained file an editor and a repository can see
//! ([0017](../docs/decisions/0017-servers-and-skills-are-configured-in-the-window.md)).
//! The window writes files; the scan re-indexes them.
//!
//! **Why not put them all in the system prompt.** Every token in it is paid for
//! on every round of every turn, and a tool-calling turn has two rounds. Ten
//! skills of a page each would cost more per turn than most turns contain. So
//! the *summaries* are always visible and the *bodies* are a tool call --
//! which is the same trade `exposed` makes for MCP tools.
//!
//! **The tool reads nothing from disk.** Enabled bodies are held in memory and
//! the tool looks up a name in that list, so a model asking for
//! `../../.ssh/id_rsa` finds no such skill rather than finding a file. There is
//! no path in the tool's arguments at all. The window *does* write files, and
//! every name it is given goes through [`path_for`], which refuses anything
//! that is not a plain name.
//!
//! These are prompt-level: text, not code. Something that *runs* is an MCP
//! server ([0002](../docs/decisions/0002-tools-behind-a-trait-mcp-via-one-adapter.md)),
//! and the roadmap rejects a second extension mechanism for good reasons.

use crate::db;
use crate::tool::{Host, Latency, Tool, ToolCtx, ToolOutcome, ToolSpec};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

/// Cap on a summary, which sits in the model's prompt on every single round.
/// One line, because the catalogue is a menu and not the meal.
const SUMMARY_MAX: usize = 200;

/// Cap on a body, which lands in the prompt for the rest of the turn once it is
/// asked for. Generous -- this is text the user wrote for themselves, not a
/// server's claims about itself -- but not unbounded, because a stray megabyte
/// would blow the context window and read as IRA going mute.
const BODY_MAX: usize = 16 * 1024;

/// The name the `skill` tool is registered under.
pub const TOOL: &str = "skill";

pub struct Skill {
    pub name: String,
    pub summary: String,
    pub body: String,
    /// Files this skill carries, if it is a folder rather than a lone file.
    /// Named in the body it hands back, and fetched one at a time.
    pub files: Vec<String>,
}

/// The enabled skills, in name order. An `RwLock` rather than a `OnceLock`
/// because the window can add, edit and switch them while IRA runs; reads
/// happen on every round and writes when someone presses Save.
fn cell() -> &'static RwLock<Vec<Skill>> {
    static CELL: OnceLock<RwLock<Vec<Skill>>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(Vec::new()))
}

/// Where the `.md` files live. Resolved once, so a reload triggered from the
/// window does not have to be told again.
fn dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        PathBuf::from(std::env::var("IRA_SKILLS").unwrap_or_else(|_| "skills".into()))
    })
}

pub fn count() -> usize {
    cell().read().map(|s| s.len()).unwrap_or(0)
}

/// Every skill on disk, on or off, for the settings window. Bodies are not in
/// here -- the window asks for one at a time.
pub fn index() -> Vec<db::SkillRow> {
    db::skills().unwrap_or_else(|e| {
        tracing::error!("could not read the skill index: {e:#}");
        Vec::new()
    })
}

/// One skill's text, read from its file rather than from memory: the window
/// edits files, so disk is the truth even for a disabled skill that was never
/// loaded into the catalogue.
pub fn body_of(name: &str) -> Result<String> {
    let path = path_for(name)?;
    std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
}

/// Whether a name is safe to turn into a path.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The file a skill name refers to.
///
/// Two shapes are allowed, and this picks whichever exists:
///
/// ```text
///   skills/standup.md             a skill that is only instructions
///   skills/standup/SKILL.md       a skill with files beside it
/// ```
///
/// The second is [0016](../docs/decisions/0016-skills-are-markdown-loaded-by-a-tool-call.md)'s
/// own "what would change this": a skill that wants to carry a template or a
/// checklist. Writing still produces the flat form unless the directory is
/// already there, so nobody gets a folder they did not ask for.
///
/// Refuses anything that is not a plain name, so a name arriving over the
/// settings port cannot climb out of the directory or pick up an extension of
/// its own. Every read and write in this module goes through here.
fn path_for(name: &str) -> Result<PathBuf> {
    if !plain(name) {
        return Err(anyhow!(
            "a skill name is letters, digits, dashes and underscores: {name:?}"
        ));
    }
    let bundled = dir().join(name).join("SKILL.md");
    if bundled.is_file() {
        return Ok(bundled);
    }
    Ok(dir().join(format!("{name}.md")))
}

/// What a bundled skill carries beside its instructions, newest names last.
///
/// Listed rather than read: the model is told these exist and can ask for one,
/// which keeps a 200 KB template out of every prompt that merely mentions the
/// skill.
fn bundled_files(name: &str) -> Vec<String> {
    let folder = dir().join(name);
    let Ok(entries) = std::fs::read_dir(&folder) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        // SKILL.md is the skill itself, not something it carries.
        .filter(|f| f != "SKILL.md")
        .collect();
    out.sort();
    out
}

/// One file from a skill's folder.
///
/// The two names are checked separately and joined here, so neither can
/// contribute a `..`: the skill must be a plain name, and the file must be a
/// plain filename with no separator in it at all.
pub fn bundled_file(skill: &str, file: &str) -> Result<String> {
    if !plain(skill) {
        return Err(anyhow!("no such skill: {skill:?}"));
    }
    let safe_file = !file.is_empty()
        && file.len() <= 128
        && !file.contains(['/', '\\'])
        && file != "."
        && file != ".."
        && !file.starts_with('.');
    if !safe_file {
        return Err(anyhow!("no such file: {file:?}"));
    }
    let path = dir().join(skill).join(file);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    Ok(truncate(&text, BODY_MAX))
}

/// Writes a skill's file and reloads.
///
/// Creates `skills/` if it is not there, so the first skill written from the
/// window needs no directory made by hand. The description goes into the file's
/// front matter rather than only into the database, because the file is the
/// thing that has to stand on its own.
pub fn write(name: &str, description: &str, body: &str) -> Result<()> {
    let path = path_for(name)?;
    let body = body.trim();
    if body.is_empty() {
        return Err(anyhow!("a skill with no text has nothing to load"));
    }
    std::fs::create_dir_all(dir()).with_context(|| format!("create {}", dir().display()))?;

    let description = description.trim().replace(['\r', '\n'], " ");
    let doc = if description.is_empty() {
        format!("{body}\n")
    } else {
        format!("---\ndescription: {}\n---\n\n{body}\n", quoted(&description))
    };
    // Written beside the target and renamed, so an interrupted save leaves the
    // old skill rather than half of the new one.
    let temp = path.with_extension("md.new");
    std::fs::write(&temp, doc).with_context(|| format!("write {}", temp.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("replace {}", path.display()))?;

    reload();
    Ok(())
}

/// A front-matter value that survives a quote or a colon in it. Without this a
/// description like `Use when: sending mail` writes a file that reads back as a
/// different key.
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The inverse of [`quoted`], and forgiving of front matter nobody quoted.
///
/// A double-quoted value is unescaped; anything else is taken literally, minus
/// surrounding single quotes. Hand-written front matter mostly has no quotes at
/// all, and a backslash in it is far more likely to be a Windows path than an
/// escape someone intended.
fn unquote(value: &str) -> String {
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.trim_matches('\'').to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next().unwrap_or('\\')),
            c => out.push(c),
        }
    }
    out
}

/// Writes a skill file verbatim, front matter and all, then reloads.
///
/// What `ira skill add <name> <file.md>` uses: a skill someone else wrote
/// already has its own front matter, and re-generating it from a parsed
/// description would drop anything this parser does not know about.
pub fn write_raw(name: &str, text: &str) -> Result<()> {
    let path = path_for(name)?;
    if parse(name, text).is_none() {
        return Err(anyhow!("a skill with no text has nothing to load"));
    }
    std::fs::create_dir_all(dir()).with_context(|| format!("create {}", dir().display()))?;
    let temp = path.with_extension("md.new");
    std::fs::write(&temp, text).with_context(|| format!("write {}", temp.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("replace {}", path.display()))?;
    reload();
    Ok(())
}

/// Deletes a skill's file, then reloads -- which is what removes its row.
pub fn delete(name: &str) -> Result<()> {
    let path = path_for(name)?;
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        // Already gone is the outcome asked for; the row still needs clearing,
        // which the reload below does.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("remove {}", path.display())))
        }
    }
    reload();
    Ok(())
}

/// Turns one skill on or off. Off means the model is never told it exists.
pub fn enable(name: &str, on: bool) -> Result<()> {
    db::skill_enable(name, on)?;
    reload();
    Ok(())
}

/// Rescans the directory, re-indexes it, and swaps the enabled set into memory.
/// Called once at start-up and after every edit from the window.
pub fn reload() {
    let mut found = Vec::new();
    let dir = dir();
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                // Either `standup.md`, or `standup/SKILL.md` for a skill that
                // carries files. Both resolve to the same name.
                let (path, name) = {
                    let p = entry.path();
                    if p.is_dir() {
                        let inner = p.join("SKILL.md");
                        if !inner.is_file() {
                            continue;
                        }
                        let Some(n) = p.file_name().and_then(|s| s.to_str()) else {
                            continue;
                        };
                        (inner, n.to_string())
                    } else {
                        if p.extension().is_none_or(|e| e != "md") {
                            continue;
                        }
                        let Some(n) = p.file_stem().and_then(|s| s.to_str()) else {
                            continue;
                        };
                        (p.clone(), n.to_string())
                    }
                };
                let name = name.as_str();
                match std::fs::read_to_string(&path) {
                    Ok(text) => match parse(name, &text) {
                        Some(mut skill) => {
                            skill.files = bundled_files(name);
                            found.push((skill, path.display().to_string()))
                        }
                        // An empty file is a skill with nothing to say. Saying
                        // so beats it silently never being chosen.
                        None => tracing::warn!(skill = name, "skill is empty, ignoring it"),
                    },
                    Err(e) => tracing::error!("could not read {}: {e}", path.display()),
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::error!("could not read {}: {e}", dir.display()),
    }

    // Sorted, so the catalogue in the prompt does not change order between
    // start-ups for no reason -- directory order is not stable across platforms
    // and an unstable prompt is an unstable answer.
    found.sort_by(|a, b| a.0.name.cmp(&b.0.name));

    // The index first: the window lists every skill on disk, on or off, so a
    // skill has to be indexed before anyone can switch it.
    let names: Vec<String> = found.iter().map(|(s, _)| s.name.clone()).collect();
    for (skill, path) in &found {
        if let Err(e) = db::skill_seen(&skill.name, path, &skill.summary) {
            tracing::error!("could not index {}: {e:#}", skill.name);
        }
    }
    if let Err(e) = db::skills_prune(&names) {
        tracing::error!("could not prune the skill index: {e:#}");
    }

    let off: Vec<String> = index()
        .into_iter()
        .filter(|r| !r.enabled)
        .map(|r| r.name)
        .collect();
    let enabled: Vec<Skill> = found
        .into_iter()
        .map(|(s, _)| s)
        .filter(|s| !off.contains(&s.name))
        .collect();

    tracing::info!(
        on = enabled.len(),
        off = off.len(),
        names = ?enabled.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        "skills loaded"
    );
    if let Ok(mut c) = cell().write() {
        *c = enabled;
    }
}

/// Registers or unregisters the `skill` tool to match what is loaded.
///
/// It comes and goes with the skills themselves: a tool offering an empty list
/// is one the model can see and cannot use. Called after every reload, because
/// the tool's description *is* the catalogue and has to be re-read.
pub fn sync_registry(host: &Host) {
    if count() > 0 {
        host.add(Arc::new(SkillTool));
    } else {
        host.remove(TOOL);
    }
}

/// One skill from its file. `None` when there is no body worth loading.
///
/// Front matter is two delimiter lines and `key: value` between them. Not a
/// YAML parser, and deliberately: the one key that matters is `description`,
/// and a dependency that can parse anchors and multi-line block scalars to read
/// one string is a dependency doing far more than is wanted.
fn parse(name: &str, text: &str) -> Option<Skill> {
    let (front, body) = split_front_matter(text);

    let described = front.and_then(|f| {
        f.lines()
            .filter_map(|l| l.trim().strip_prefix("description:"))
            .map(|v| unquote(v.trim()))
            .find(|v| !v.is_empty())
    });

    let body = body.trim();
    if body.is_empty() {
        return None;
    }

    // A skill with no description is still usable: its first line says what it
    // is far more often than not, and the alternative -- refusing to load it --
    // hides the skill for a missing field the user did not know to write.
    let summary = described.unwrap_or_else(|| {
        body.lines()
            .map(|l| l.trim().trim_start_matches('#').trim())
            .find(|l| !l.is_empty())
            .unwrap_or(name)
            .to_string()
    });

    Some(Skill {
        name: name.to_string(),
        summary: truncate(&summary, SUMMARY_MAX),
        body: truncate(body, BODY_MAX),
        files: Vec::new(),
    })
}

/// Splits leading `---` front matter from the body. Tolerates CRLF, because a
/// skill is a file the user wrote in whatever editor they have.
fn split_front_matter(text: &str) -> (Option<&str>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text.strip_prefix("---") else {
        return (None, text);
    };
    let rest = rest.trim_start_matches('\r').strip_prefix('\n').unwrap_or(rest);
    // The closing delimiter is a line of its own, so the search is for a
    // newline before it -- otherwise a `---` rule inside the body would end the
    // front matter early and swallow half the skill.
    match rest
        .match_indices("---")
        .find(|(i, _)| *i == 0 || rest[..*i].ends_with('\n'))
    {
        Some((i, _)) => {
            let after = &rest[i + 3..];
            let after = after.trim_start_matches('\r').strip_prefix('\n').unwrap_or(after);
            (Some(&rest[..i]), after)
        }
        // An opening delimiter with no closing one is a body starting with a
        // rule, not front matter that ate the file.
        None => (None, text),
    }
}

/// Cut on a character boundary, and say that it was cut. Silently truncated
/// instructions read as instructions the model ignored.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n[truncated at {max} bytes]", &s[..end])
}

/// The one built-in that only exists when the user has written a skill.
pub struct SkillTool;

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn spec(&self) -> ToolSpec {
        let loaded = cell().read();
        let skills: &[Skill] = loaded.as_deref().map(|v| &v[..]).unwrap_or(&[]);

        let mut description = String::from(
            "Instructions the user wrote for a particular kind of task. When one of these \
             covers what you have been asked, call this first and follow what it says. \
             Available:",
        );
        for s in skills {
            description.push_str(&format!("\n- {}: {}", s.name, s.summary));
            if !s.files.is_empty() {
                description.push_str(&format!(" (carries {})", s.files.join(", ")));
            }
        }
        if skills.iter().any(|s| !s.files.is_empty()) {
            description.push_str(
                "\n\nSome carry files. Ask for one with `file`, and only when the \
                 instructions tell you to -- they are not loaded otherwise.",
            );
        }

        ToolSpec {
            name: TOOL.into(),
            description,
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        // Enumerated rather than free text: the model cannot ask
                        // for one that does not exist, which is the whole class
                        // of "load ../../secrets" this tool would otherwise
                        // have to defend against.
                        "enum": skills.iter().map(|s| &s.name).collect::<Vec<_>>(),
                    },
                    "file": {
                        "type": "string",
                        "description": "One of the files that skill carries. \
                                        Omit to get the instructions themselves.",
                        // Enumerated for the same reason as the name. A skill
                        // with no files contributes nothing, so this is empty
                        // unless something is genuinely fetchable.
                        "enum": skills.iter().flat_map(|s| &s.files).collect::<Vec<_>>(),
                    }
                },
                "required": ["name"]
            }),
            mutates: false,
            latency: Latency::Fast,
            confirm: None,
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| anyhow!("skill needs a `name`"))?;
        let loaded = cell()
            .read()
            .map_err(|_| anyhow!("the skill list is poisoned"))?;
        let skill = loaded
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| anyhow!("no such skill: {name}"))?;

        // A file, if one was asked for and this skill actually carries it. The
        // membership check is what stops one skill reaching another's files --
        // the enum in the schema is the union across every skill, so it alone
        // is not enough.
        if let Some(file) = args["file"].as_str().filter(|f| !f.is_empty()) {
            if !skill.files.iter().any(|f| f == file) {
                return Err(anyhow!("{name} does not carry {file}"));
            }
            let name = skill.name.clone();
            // Dropped before the read: the list is behind an RwLock every turn
            // wants, and a slow disk must not hold it.
            drop(loaded);
            return Ok(ToolOutcome::Answer(bundled_file(&name, file)?));
        }

        let mut body = skill.body.clone();
        if !skill.files.is_empty() {
            body.push_str(&format!(
                "\n\nFiles you can ask for by name: {}.",
                skill.files.join(", ")
            ));
        }
        Ok(ToolOutcome::Answer(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Front matter is the format users will actually write, and every part of
    /// it is optional in practice: CRLF from a Windows editor, quotes around
    /// the value, a missing description, no front matter at all.
    #[test]
    fn front_matter_is_read_however_it_was_written() {
        let s = parse("standup", "---\ndescription: How I write one.\n---\nThree lines.").unwrap();
        assert_eq!(s.summary, "How I write one.");
        assert_eq!(s.body, "Three lines.");

        let crlf = parse("a", "---\r\ndescription: \"Quoted.\"\r\n---\r\nBody.\r\n").unwrap();
        assert_eq!(crlf.summary, "Quoted.");
        assert_eq!(crlf.body.trim(), "Body.");

        // No description: the first real line stands in, heading marks stripped.
        let bare = parse("notes", "# Taking notes\n\nDo it like this.").unwrap();
        assert_eq!(bare.summary, "Taking notes");
        assert_eq!(bare.body, "# Taking notes\n\nDo it like this.");

        // No front matter at all.
        let plain = parse("plain", "Just instructions.").unwrap();
        assert_eq!(plain.summary, "Just instructions.");
    }

    /// What the window writes must be what the scan reads back. A description
    /// with a colon or a quote in it is the case that silently writes a file
    /// whose front matter parses as something else.
    #[test]
    fn a_description_the_window_wrote_survives_being_read_back() {
        for description in [
            "Use when: sending mail",
            r#"The "usual" thing"#,
            r"A backslash \ in it",
        ] {
            let doc = format!("---\ndescription: {}\n---\n\nBody.\n", quoted(description));
            let back = parse("x", &doc).unwrap();
            assert_eq!(back.summary, description, "in {doc:?}");
        }
    }

    /// A `---` rule in the body must not be mistaken for the closing delimiter,
    /// which would silently drop everything above it -- the skill would load,
    /// and be half of itself.
    #[test]
    fn a_horizontal_rule_does_not_end_the_front_matter_early() {
        let s = parse("x", "---\ndescription: D\n---\nOne.\n\n---\n\nTwo.").unwrap();
        assert_eq!(s.summary, "D");
        assert!(s.body.contains("One."), "{:?}", s.body);
        assert!(s.body.contains("Two."), "{:?}", s.body);

        // An opening delimiter that is never closed is a body, not front matter
        // that ate the file.
        let unclosed = parse("y", "---\nnot really front matter").unwrap();
        assert!(unclosed.body.starts_with("---"), "{:?}", unclosed.body);
    }

    /// An empty file is a skill that can never be usefully loaded, and one the
    /// model would otherwise be offered.
    #[test]
    fn an_empty_skill_is_not_a_skill() {
        assert!(parse("empty", "").is_none());
        assert!(parse("empty", "---\ndescription: D\n---\n\n  \n").is_none());
    }

    /// Both caps cut on a character boundary and say they did. A silent
    /// truncation reads as instructions the model chose to ignore.
    #[test]
    fn an_oversized_skill_is_cut_and_says_so() {
        let long = "é".repeat(BODY_MAX);
        let s = parse("big", &long).unwrap();
        assert!(s.body.len() <= BODY_MAX + 32);
        assert!(s.body.ends_with("bytes]"), "{:?}", &s.body[s.body.len() - 40..]);
        assert!(s.summary.len() <= SUMMARY_MAX + 32);
    }

    /// The window takes a name over a loopback port and turns it into a path.
    /// This is the only place that happens, so this is where traversal has to
    /// stop -- a name that is not a plain name never becomes a file.
    #[test]
    fn a_name_from_the_window_cannot_climb_out_of_the_directory() {
        for bad in [
            "../../.ssh/id_rsa",
            "../secrets",
            "a/b",
            r"a\b",
            "",
            "with space",
            "dots.and.more",
        ] {
            assert!(path_for(bad).is_err(), "{bad:?} was accepted as a skill name");
        }
        for good in ["standup", "take-notes", "meeting_notes", "v2"] {
            assert!(path_for(good).is_ok(), "{good:?} was refused");
        }
    }

    /// A bundled skill joins two names into a path, so both halves have to be
    /// checked. The enum in the schema is the union across every skill, which
    /// means it alone would let one skill fetch another's file -- the
    /// membership check in `call` is what actually stops that, and this covers
    /// the half that turns a filename into a path.
    #[test]
    fn a_bundled_filename_cannot_climb_out_of_its_skill() {
        for bad in [
            "../../../etc/passwd",
            "../standup.md",
            "sub/dir.md",
            r"sub\dir.md",
            ".ssh",
            ".",
            "..",
            "",
        ] {
            assert!(
                bundled_file("standup", bad).is_err(),
                "{bad:?} was accepted as a bundled filename"
            );
        }
        // And a bad skill name is refused before the filename matters at all.
        assert!(bundled_file("../secrets", "template.md").is_err());
    }

    /// The tool takes a name from a fixed list, never a path, so no argument it
    /// accepts can reach a file at all.
    #[tokio::test]
    async fn a_skill_that_does_not_exist_is_an_error_not_a_file_read() {
        let ctx = ToolCtx {
            transcript: String::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
        };

        let out = SkillTool.call(json!({"name": "../../.ssh/id_rsa"}), &ctx).await;
        assert!(out.is_err(), "a path must not resolve to anything");

        let schema = SkillTool.spec().schema;
        assert!(
            schema["properties"]["name"]["enum"].is_array(),
            "the model must be given a closed list of names"
        );
    }
}
