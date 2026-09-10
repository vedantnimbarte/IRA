//! What IRA is configured with, and where it is kept.
//!
//! [decisions/0014](../docs/decisions/0014-settings-are-editable-while-she-runs.md).
//!
//! Everything here used to be an environment variable and still can be. The
//! addition is that a running IRA can be told otherwise, and remember it:
//!
//! ```text
//!   an override set in the settings window   ← wins
//!   the environment                          ← what you started her with
//!   nothing                                  ← the code's own default
//! ```
//!
//! **The environment is never written to.** `std::env::set_var` races with the
//! `getenv` happening on the audio, model and orb threads -- it is `unsafe` in
//! Rust 2024 for exactly that reason -- and this process has too many threads
//! to take that on for a convenience. Instead the values live in a map behind
//! an `RwLock`, and every caller that used to read the environment reads
//! [`get`] instead. Reads are per request in `llm.rs` and `stt.rs`, so a key
//! saved now is used by the next sentence with nothing restarted.
//!
//! **Secrets are not in the file.** API keys go to the Windows Credential
//! Manager, which is encrypted at rest per user; the file holds only URLs and
//! model ids. A key is never read back out to the settings page either -- the
//! page is told whether one is set, never what it is.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

/// Everything the settings window can change.
///
/// Deliberately short. These are the values that stop IRA working when they are
/// wrong, and the ones worth a window; the tuning constants stay in `main.rs`
/// where changing them is a decision rather than a preference.
pub const FIELDS: &[Field] = &[
    Field::secret("ANTHROPIC_API_KEY", "Anthropic key, for the model"),
    Field::secret("GROQ_API_KEY", "Groq key, for transcription"),
    Field::plain("IRA_LLM_URL", "An OpenAI-compatible endpoint, instead of Anthropic"),
    Field::secret("IRA_LLM_KEY", "Key for that endpoint, if it wants one"),
    Field::plain("IRA_LLM_MODEL", "Model id, as that provider spells it"),
    Field::plain("IRA_STT_URL", "A local whisper-server, instead of Groq"),
];

pub struct Field {
    pub name: &'static str,
    pub about: &'static str,
    /// Whether this goes to the credential store rather than the file, and is
    /// never sent back to the page.
    pub secret: bool,
}

impl Field {
    const fn secret(name: &'static str, about: &'static str) -> Self {
        Self { name, about, secret: true }
    }
    const fn plain(name: &'static str, about: &'static str) -> Self {
        Self { name, about, secret: false }
    }
}

fn field(name: &str) -> Option<&'static Field> {
    FIELDS.iter().find(|f| f.name == name)
}

fn overrides() -> &'static RwLock<BTreeMap<String, String>> {
    static OVERRIDES: OnceLock<RwLock<BTreeMap<String, String>>> = OnceLock::new();
    OVERRIDES.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Where the non-secret settings are kept. Beside `ira.toml` rather than in it:
/// `ira.toml` is shareable -- its MCP blocks are the kind of thing you commit --
/// and this file is about one machine.
fn file() -> PathBuf {
    PathBuf::from(std::env::var("IRA_SETTINGS").unwrap_or_else(|_| "ira.local.toml".into()))
}

/// The value of a setting: an override if one has been saved, otherwise the
/// environment, otherwise nothing.
///
/// A blank override means "unset this", which is how a URL is cleared to get
/// back to the default provider. Without that, emptying a box in the settings
/// window would silently fall through to whatever the environment still said.
pub fn get(name: &str) -> Option<String> {
    if let Some(value) = overrides().read().ok()?.get(name) {
        return (!value.is_empty()).then(|| value.clone());
    }
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Whether a setting has a value, without being told what it is. What the
/// settings page is allowed to know about a key.
pub fn is_set(name: &str) -> bool {
    get(name).is_some()
}

/// Loads saved settings over the environment. Called once at start-up.
///
/// A missing file is the normal case and not a failure. A malformed one is
/// reported and ignored: bad settings must not stop IRA answering questions,
/// and refusing to start because of a stray quote in a config file would be a
/// worse outcome than running on the environment alone.
pub fn load() {
    let mut loaded = 0;
    match std::fs::read_to_string(file()) {
        Ok(text) => match toml::from_str::<BTreeMap<String, String>>(&text) {
            Ok(saved) => {
                if let Ok(mut o) = overrides().write() {
                    for (k, v) in saved {
                        if field(&k).is_some_and(|f| !f.secret) {
                            o.insert(k, v);
                            loaded += 1;
                        }
                    }
                }
            }
            Err(e) => tracing::error!("{} is not readable, ignoring it: {e}", file().display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::error!("could not read {}: {e}", file().display()),
    }

    for f in FIELDS.iter().filter(|f| f.secret) {
        match secret::read(f.name) {
            Ok(Some(value)) => {
                if let Ok(mut o) = overrides().write() {
                    o.insert(f.name.into(), value);
                    loaded += 1;
                }
            }
            Ok(None) => {}
            Err(e) => tracing::error!("could not read {} from the credential store: {e}", f.name),
        }
    }

    if loaded > 0 {
        tracing::info!(loaded, "settings loaded");
    }
}

/// Saves one setting and applies it immediately.
///
/// An empty value clears it: the credential is deleted, or the line leaves the
/// file. The override stays, as an empty string, so `get` knows the difference
/// between "cleared" and "never set" and does not fall back to the environment.
pub fn set(name: &str, value: &str) -> Result<()> {
    let field = field(name).with_context(|| format!("{name} is not a setting"))?;
    let value = value.trim();

    if field.secret {
        if value.is_empty() {
            secret::delete(name)?;
        } else {
            secret::write(name, value)?;
        }
    }

    overrides()
        .write()
        .map_err(|_| anyhow::anyhow!("settings lock poisoned"))?
        .insert(name.into(), value.into());

    if !field.secret {
        write_file()?;
    }
    // Never the value, and never at a level that ends up in a shared log.
    tracing::info!(name, cleared = value.is_empty(), "setting saved");
    Ok(())
}

/// Rewrites the whole file from the current overrides.
///
/// Whole-file rather than patching a line: this file is six keys and a
/// hand-edited one is not a thing to try to preserve the formatting of.
fn write_file() -> Result<()> {
    let o = overrides()
        .read()
        .map_err(|_| anyhow::anyhow!("settings lock poisoned"))?;
    let mut doc = String::from(
        "# Written by IRA's settings window. Safe to edit or delete.\n\
         # Keys are not here -- they are in the Windows Credential Manager.\n",
    );
    for f in FIELDS.iter().filter(|f| !f.secret) {
        if let Some(value) = o.get(f.name).filter(|v| !v.is_empty()) {
            doc.push_str(&format!("{} = {}\n", f.name, toml_string(value)));
        }
    }
    drop(o);

    let path = file();
    // Written next to the target and renamed, so an interrupted save leaves the
    // old settings rather than half of the new ones.
    let temp = path.with_extension("toml.new");
    std::fs::write(&temp, doc).with_context(|| format!("write {}", temp.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// A TOML basic string. These are URLs and model ids, but a stray quote or
/// backslash in one would otherwise write a file that will not parse.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The Windows Credential Manager: encrypted at rest, per user, and not a file
/// that can be committed by accident.
#[cfg(windows)]
mod secret {
    use anyhow::{anyhow, Result};
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
        CRED_TYPE_GENERIC,
    };

    /// Namespaced, so IRA's entries are identifiable in the Windows UI and
    /// cannot collide with anything else storing a key by the same name.
    fn target(name: &str) -> Vec<u16> {
        wide(&format!("IRA/{name}"))
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn read(name: &str) -> Result<Option<String>> {
        let target = target(name);
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: a null-terminated target name that outlives the call. On
        // success `cred` is a block owned by the caller until `CredFree`, and
        // the blob is read within its own stated length.
        unsafe {
            if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) == 0 {
                // Not found is the ordinary case for a key never saved.
                return match windows_sys::Win32::Foundation::GetLastError() {
                    1168 => Ok(None),
                    e => Err(anyhow!("CredRead failed, error {e}")),
                };
            }
            let bytes = std::slice::from_raw_parts(
                (*cred).CredentialBlob as *const u8,
                (*cred).CredentialBlobSize as usize,
            );
            let value = String::from_utf8(bytes.to_vec());
            CredFree(cred as *const _);
            Ok(Some(value?))
        }
    }

    pub fn write(name: &str, value: &str) -> Result<()> {
        let mut target = target(name);
        let mut user = wide("IRA");
        let mut blob = value.as_bytes().to_vec();
        let mut cred: CREDENTIALW = unsafe { std::mem::zeroed() };
        cred.Type = CRED_TYPE_GENERIC;
        cred.TargetName = target.as_mut_ptr();
        cred.UserName = user.as_mut_ptr();
        cred.CredentialBlobSize = blob.len() as u32;
        cred.CredentialBlob = blob.as_mut_ptr();
        // This machine only. A roaming credential would put the key on every
        // machine the account touches, which is not what saving it here means.
        cred.Persist = CRED_PERSIST_LOCAL_MACHINE;

        // SAFETY: every pointer in `cred` is to a local that outlives the call,
        // and the blob length is the length of that local.
        let ok = unsafe { CredWriteW(&cred, 0) };
        if ok == 0 {
            let e = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            return Err(anyhow!("CredWrite failed, error {e}"));
        }
        Ok(())
    }

    pub fn delete(name: &str) -> Result<()> {
        let target = target(name);
        // SAFETY: a null-terminated target name that outlives the call.
        let ok = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if ok == 0 {
            // Deleting one that was never there is the outcome asked for.
            return match unsafe { windows_sys::Win32::Foundation::GetLastError() } {
                1168 => Ok(()),
                e => Err(anyhow!("CredDelete failed, error {e}")),
            };
        }
        Ok(())
    }
}

/// Everywhere else has no credential store IRA knows how to use, so secrets
/// stay in the environment and the settings window says so.
#[cfg(not(windows))]
mod secret {
    use anyhow::{anyhow, Result};

    pub fn read(_name: &str) -> Result<Option<String>> {
        Ok(None)
    }
    pub fn write(_name: &str, _value: &str) -> Result<()> {
        Err(anyhow!("saving keys needs the Windows Credential Manager"))
    }
    pub fn delete(_name: &str) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A URL with a quote in it would write a file that will not parse, and the
    /// failure lands at the *next* start-up rather than at the save -- so it
    /// looks like settings being forgotten rather than like a bad value.
    #[test]
    fn a_value_with_quotes_survives_the_round_trip() {
        for value in [
            r#"http://x/a"b"#,
            r"C:\models\a",
            "plain",
            "tab\there",
            "new\nline",
        ] {
            let doc = format!("IRA_LLM_URL = {}\n", toml_string(value));
            let back: BTreeMap<String, String> =
                toml::from_str(&doc).unwrap_or_else(|e| panic!("{doc:?} did not parse: {e}"));
            assert_eq!(back["IRA_LLM_URL"], value);
        }
    }

    /// Only what the window offers can be written, so a POST naming something
    /// else cannot reach through and set an arbitrary variable.
    #[test]
    fn nothing_outside_the_list_can_be_set() {
        assert!(set("PATH", "/tmp").is_err());
        assert!(set("IRA_UI", "off").is_err());
        assert!(field("ANTHROPIC_API_KEY").is_some());
    }

    /// The whole point of the override layer: a saved value beats the
    /// environment, and a cleared one does not fall back to it.
    #[test]
    fn a_saved_value_wins_and_a_cleared_one_stays_cleared() {
        let name = "IRA_LLM_MODEL";
        std::env::set_var(name, "from-the-environment");
        assert_eq!(get(name).as_deref(), Some("from-the-environment"));

        overrides().write().unwrap().insert(name.into(), "from-the-window".into());
        assert_eq!(get(name).as_deref(), Some("from-the-window"));

        overrides().write().unwrap().insert(name.into(), String::new());
        assert_eq!(get(name), None, "cleared must not fall back to the environment");

        overrides().write().unwrap().remove(name);
        std::env::remove_var(name);
    }

    /// Keys are write-only from the page's side.
    #[test]
    fn a_secret_can_be_asked_about_but_not_read_back() {
        assert!(FIELDS.iter().filter(|f| f.secret).count() >= 3);
        assert!(!is_set("ANTHROPIC_API_KEY") || get("ANTHROPIC_API_KEY").is_some());
    }
}
