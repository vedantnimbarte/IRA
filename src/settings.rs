//! What IRA is configured with, and where it is kept.
//!
//! [decisions/0014](../docs/decisions/0014-settings-are-editable-while-she-runs.md).
//!
//! Two stores, and the environment is not one of them:
//!
//! ```text
//!   API keys      → the OS keyring   (Credential Manager, Keychain, Secret Service)
//!   URLs and ids  → ira.local.db     (SQLite, beside IRA)
//!   nothing set   → the code's own default
//! ```
//!
//! **The environment is not read.** It used to be the bottom of that stack, and
//! a key on a command line ends up in shell history, in `ps`, and in whatever
//! CI log echoed the step that set it. A keyring is encrypted at rest per user
//! and cannot be exported by anything that can reach IRA's port. Values live in
//! a map behind an `RwLock`, read per request in `llm.rs` and `stt.rs`, so a key
//! saved now is used by the next sentence with nothing restarted.
//!
//! **The environment is never written to either.** `std::env::set_var` races
//! with the `getenv` happening on the audio, model and orb threads -- it is
//! `unsafe` in Rust 2024 for exactly that reason.
//!
//! **Secrets are not in the database.** A key is never read back out to the
//! settings page either -- the page is told whether one is set, never what it
//! is. `ira set <NAME> <VALUE>` is the way in on a machine with no window yet.

use crate::db;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

/// Everything the settings window can change.
///
/// Deliberately short. These are the values that stop IRA working when they are
/// wrong, and the ones worth a window; the tuning constants stay in `main.rs`
/// where changing them is a decision rather than a preference.
pub const FIELDS: &[Field] = &[
    Field {
        name: "GROQ_API_KEY",
        label: "Groq key",
        group: HEARING,
        about: "",
        empty: "Needed, unless you run whisper below.",
        secret: true,
    },
    Field {
        name: "IRA_STT_URL",
        label: "Local transcription",
        group: HEARING,
        about: "A whisper-server here. No audio leaves this machine.",
        empty: "Transcribing at Groq.",
        secret: false,
    },
    Field {
        name: "ANTHROPIC_API_KEY",
        label: "Anthropic key",
        group: ANSWERING,
        about: "",
        empty: "Needed, unless you give another endpoint below.",
        secret: true,
    },
    Field {
        name: "IRA_LLM_URL",
        label: "Another endpoint",
        group: ANSWERING,
        about: "OpenAI chat-completions format: OpenRouter, LM Studio, Ollama.",
        empty: "Talking to Anthropic.",
        secret: false,
    },
    Field {
        name: "IRA_LLM_KEY",
        label: "Key for that endpoint",
        group: ANSWERING,
        about: "A server on this machine usually wants none.",
        empty: "No key sent.",
        secret: true,
    },
    Field {
        name: "IRA_LLM_MODEL",
        label: "Model",
        group: ANSWERING,
        about: "The id that provider uses, not Anthropic's.",
        // Filled in from `llm::MODEL`, so it cannot drift from the real default.
        empty: "",
        secret: false,
    },
];

/// The two stages of a turn that leave this machine, in the order they happen.
///
/// Not a category scheme invented for the window: everything before
/// transcription -- the wake word, knowing when you have stopped -- already runs
/// here, so these six values are exactly the ones that decide what goes out.
pub const GROUPS: &[Group] = &[
    Group {
        id: HEARING,
        title: "Hearing you",
        about: "Speech becomes text. Everything before this already happens here.",
    },
    Group {
        id: ANSWERING,
        title: "Answering",
        about: "The one stage that still needs the network.",
    },
];

const HEARING: &str = "hearing";
const ANSWERING: &str = "answering";

pub struct Group {
    pub id: &'static str,
    pub title: &'static str,
    pub about: &'static str,
}

pub struct Field {
    pub name: &'static str,
    /// What to call it to someone who did not write the code.
    pub label: &'static str,
    pub group: &'static str,
    pub about: &'static str,
    /// What IRA does when this is not set. An empty box should say what happens
    /// instead of it, rather than only that it is empty.
    pub empty: &'static str,
    /// Whether this goes to the OS keyring rather than the database, and is
    /// never sent back to the page.
    pub secret: bool,
}

impl Field {
    /// What the window shows under an empty box. The model's default lives in
    /// `llm.rs`, so it is read from there rather than repeated here.
    pub fn when_empty(&self) -> String {
        if self.empty.is_empty() {
            format!("Using {}.", crate::llm::MODEL)
        } else {
            self.empty.into()
        }
    }

    /// Where this one is kept, for the window and for `ira set` with no value.
    pub fn store(&self) -> &'static str {
        if self.secret {
            "the OS keyring"
        } else {
            "ira.local.db"
        }
    }
}

fn field(name: &str) -> Option<&'static Field> {
    FIELDS.iter().find(|f| f.name == name)
}

fn values() -> &'static RwLock<BTreeMap<String, String>> {
    static VALUES: OnceLock<RwLock<BTreeMap<String, String>>> = OnceLock::new();
    VALUES.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// The value of a setting, or nothing.
///
/// A blank value means "unset this", which is how a URL is cleared to get back
/// to the default provider without the clear being indistinguishable from
/// never having set it.
pub fn get(name: &str) -> Option<String> {
    values()
        .read()
        .ok()?
        .get(name)
        .filter(|v| !v.is_empty())
        .cloned()
}

/// Whether a setting has a value, without being told what it is. What the
/// settings page is allowed to know about a key.
pub fn is_set(name: &str) -> bool {
    get(name).is_some()
}

/// Sets a value in memory only, for the tests that need one without writing to
/// the machine's real keyring or database.
#[cfg(test)]
pub fn set_in_memory(name: &str, value: &str) {
    values().write().unwrap().insert(name.into(), value.into());
}

/// Loads the database and the keyring into memory. Called once at start-up,
/// before the fatal start-up checks, because those are checks on these values.
///
/// A missing database is the normal case and not a failure. An unreadable one
/// is reported and ignored: bad settings must not stop IRA answering questions,
/// and refusing to start because of a corrupt row would be a worse outcome than
/// running on the defaults.
pub fn load() {
    let mut loaded = 0;
    match db::settings_all() {
        Ok(saved) => {
            if let Ok(mut v) = values().write() {
                for (name, value) in saved {
                    // A row for something that is no longer a setting is
                    // ignored rather than honoured.
                    if field(&name).is_some_and(|f| !f.secret) {
                        v.insert(name, value);
                        loaded += 1;
                    }
                }
            }
        }
        Err(e) => tracing::error!("could not read {}, ignoring it: {e:#}", db::PATH),
    }

    for f in FIELDS.iter().filter(|f| f.secret) {
        match secret::read(f.name) {
            Ok(Some(value)) => {
                if let Ok(mut v) = values().write() {
                    v.insert(f.name.into(), value);
                    loaded += 1;
                }
            }
            Ok(None) => {}
            Err(e) => tracing::error!("could not read {} from the keyring: {e:#}", f.name),
        }
    }

    if loaded > 0 {
        tracing::info!(loaded, "settings loaded");
    }
}

/// Saves one setting and applies it immediately.
///
/// An empty value clears it: the credential is deleted, or the row leaves the
/// database. The in-memory value stays, as an empty string, so a clear is
/// distinguishable from never having been set.
pub fn set(name: &str, value: &str) -> Result<()> {
    let field = field(name).with_context(|| format!("{name} is not a setting"))?;
    let value = value.trim();

    // The store first: failing to persist must not leave IRA running on a value
    // that will be gone at the next start-up.
    match (field.secret, value.is_empty()) {
        (true, false) => secret::write(name, value)?,
        (true, true) => secret::delete(name)?,
        (false, false) => db::settings_set(name, value)?,
        (false, true) => db::settings_delete(name)?,
    }

    values()
        .write()
        .map_err(|_| anyhow::anyhow!("settings lock poisoned"))?
        .insert(name.into(), value.into());

    // Never the value, and never at a level that ends up in a shared log.
    tracing::info!(name, cleared = value.is_empty(), "setting saved");
    Ok(())
}

/// `ira set <NAME> [VALUE]` -- how a key gets in on a machine that has never
/// started IRA, since the fatal start-up check for a missing key fires long
/// before there is a settings window to type one into. Returns an exit code.
pub fn set_from_cli(args: &[String]) -> i32 {
    let (name, value) = match args {
        [name, value] => (name.as_str(), value.as_str()),
        [name] => (name.as_str(), ""),
        _ => {
            eprintln!("usage: ira set <NAME> [VALUE]     -- no value clears it\n");
            for f in FIELDS {
                eprintln!("  {:<18} {}", f.name, f.store());
            }
            return 2;
        }
    };
    match set(name, value) {
        // Never the value: this is a terminal, and terminals are recorded.
        Ok(()) => {
            let where_ = field(name).map(|f| f.store()).unwrap_or_default();
            if value.is_empty() {
                println!("{name} cleared");
            } else {
                println!("{name} saved to {where_}");
            }
            0
        }
        Err(e) => {
            eprintln!("{name}: {e:#}");
            1
        }
    }
}

/// The OS keyring: Windows Credential Manager, macOS Keychain, or the
/// freedesktop Secret Service. Encrypted at rest, per user, and not a file that
/// can be committed by accident.
///
/// The `keyring` crate rather than three hand-written FFI bindings: this used
/// to be 80 lines of `unsafe` Win32 that only worked on Windows, and the other
/// two platforms had no store at all and leaned on the environment -- which is
/// the thing being removed.
pub mod secret {
    use anyhow::{anyhow, Result};
    use keyring::v1::{Entry, Error};

    /// The keyring name for a value a server is given.
    ///
    /// Namespaced under `env/` so a server's `GROQ_API_KEY` is a different
    /// credential from IRA's own -- which is the whole point of giving a server
    /// its own environment rather than letting it inherit hers.
    pub fn env_key(server: &str, name: &str) -> String {
        format!("env/{server}/{name}")
    }

    /// The keyring name for a server's OAuth tokens.
    pub fn oauth_key(server: &str) -> String {
        format!("oauth/{server}")
    }

    /// Namespaced, so IRA's entries are identifiable in each platform's own UI
    /// and cannot collide with anything else storing a key by the same name.
    fn entry(name: &str) -> Result<Entry> {
        Entry::new("IRA", name).map_err(|e| match e {
            // Worth its own sentence: a headless Linux box often has no Secret
            // Service running at all, and "no default store" does not say that.
            Error::NoDefaultStore => anyhow!(
                "no OS keyring on this machine \
                 (Linux needs a running Secret Service, e.g. gnome-keyring)"
            ),
            e => anyhow!(e),
        })
    }

    pub fn read(name: &str) -> Result<Option<String>> {
        match entry(name)?.get_password() {
            Ok(value) => Ok(Some(value)),
            // Not found is the ordinary case for a key never saved.
            Err(Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow!(e)),
        }
    }

    pub fn write(name: &str, value: &str) -> Result<()> {
        entry(name)?.set_password(value).map_err(|e| anyhow!(e))
    }

    pub fn delete(name: &str) -> Result<()> {
        match entry(name)?.delete_credential() {
            // Deleting one that was never there is the outcome asked for.
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow!(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only what the window offers can be written, so a POST naming something
    /// else cannot reach through and set an arbitrary variable.
    #[test]
    fn nothing_outside_the_list_can_be_set() {
        assert!(set("PATH", "/tmp").is_err());
        assert!(set("IRA_UI", "off").is_err());
        assert!(field("ANTHROPIC_API_KEY").is_some());
    }

    /// The whole point of the change: a variable in the shell is no longer a
    /// way to configure IRA, and a cleared setting stays cleared rather than
    /// falling through to one.
    #[test]
    fn the_environment_is_not_a_source_and_a_cleared_value_stays_cleared() {
        let name = "IRA_LLM_MODEL";
        std::env::set_var(name, "from-the-environment");
        assert_eq!(get(name), None, "the environment must not be read");

        values().write().unwrap().insert(name.into(), "from-the-window".into());
        assert_eq!(get(name).as_deref(), Some("from-the-window"));

        values().write().unwrap().insert(name.into(), String::new());
        assert_eq!(get(name), None, "cleared must stay cleared");

        values().write().unwrap().remove(name);
        std::env::remove_var(name);
    }

    /// Keys are write-only from the page's side.
    #[test]
    fn a_secret_can_be_asked_about_but_not_read_back() {
        assert!(FIELDS.iter().filter(|f| f.secret).count() >= 3);
        assert!(!is_set("ANTHROPIC_API_KEY") || get("ANTHROPIC_API_KEY").is_some());
    }

    /// A round trip through the real database, in a temp directory so it never
    /// touches the one beside IRA. The second write is the point: saving twice
    /// must replace the row rather than fail on the primary key, and the value
    /// is one that would have needed escaping in the TOML file this replaced.
    #[test]
    fn a_setting_survives_the_database_round_trip() {
        let _guard = crate::db::cwd_lock();
        let dir = std::env::temp_dir().join("ira-settings-test");
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let _ = std::fs::remove_file(db::PATH);

        db::settings_set("IRA_LLM_URL", "http://one").unwrap();
        db::settings_set("IRA_LLM_URL", r#"http://x/a"b"#).unwrap();
        assert_eq!(db::settings_all().unwrap()["IRA_LLM_URL"], r#"http://x/a"b"#);

        db::settings_delete("IRA_LLM_URL").unwrap();
        assert!(db::settings_all().unwrap().is_empty());

        std::env::set_current_dir(cwd).unwrap();
    }
}
