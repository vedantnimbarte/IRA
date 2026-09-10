//! Where IRA's files live.
//!
//! Everything used to be relative to the working directory, which is right for
//! a checkout and wrong for an installed copy: an installer puts the binary
//! somewhere the user cannot write, and a Start-menu shortcut launches with a
//! working directory nobody chose. `models/` beside the binary is then a
//! read-only directory, and `ira.local.db` lands wherever Explorer felt like.
//!
//! So there is one answer to "where", resolved here, in this order:
//!
//! ```text
//!   IRA_DATA               set it and that is the answer, full stop
//!   *this* crate's checkout use the checkout, as it always did
//!   the per-user dir       %LOCALAPPDATA%\IRA, ~/.local/share/ira, ~/Library/...
//! ```
//!
//! The middle rule is what keeps `cargo run` in this repository behaving
//! exactly as it did before any of this existed: the models you fetched into
//! `./models` are still the models it loads, and an installed IRA on the same
//! machine keeps its own state somewhere else entirely.
//!
//! It reads the manifest's name rather than only looking for a `Cargo.toml`,
//! because every Rust project has one of those. The looser test gave an
//! installed IRA a separate data directory in every repository the user
//! happened to be standing in, and would have downloaded 85 MB of models into
//! somebody else's project the first time she started there.
//!
//! `IRA_DATA` is first rather than a convenience, and the tests are the reason.
//! Several of them chdir into a temp directory, which has no `Cargo.toml` and
//! would therefore fall through to the real per-user directory -- a `cargo test`
//! that quietly edits the settings of the IRA you actually use. Every such test
//! points `IRA_DATA` at its own directory instead.
//!
//! The per-setting overrides (`IRA_MODELS`, `IRA_PIPER`, `IRA_SKILLS`,
//! `IRA_TRANSCRIPT`, `IRA_CONFIG`) still win over all of this. They name one
//! file each; this names the drawer they default into.

use std::path::{Path, PathBuf};

/// The directory holding everything IRA writes and everything she downloads.
///
/// Deliberately not cached: the tests move between directories, and a `OnceLock`
/// would freeze the first test's answer for every test after it. The cost is an
/// environment read and one `stat` per call, on paths that are touched at
/// start-up and when a settings window saves -- not in the turn loop.
pub fn data() -> PathBuf {
    if let Some(dir) = std::env::var_os("IRA_DATA").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    // A checkout. `cargo run` keeps using ./models, ./ira.local.db and the rest,
    // so a repository is still a self-contained place to work.
    if in_iras_own_checkout() {
        return PathBuf::from(".");
    }
    per_user()
}

/// `data()` joined with a relative path, which is what every call site wants.
pub fn in_data(rel: impl AsRef<Path>) -> PathBuf {
    data().join(rel)
}

/// Whether the working directory is a checkout of *this* program.
///
/// The name is checked, not just the presence of a `Cargo.toml`. Any Rust
/// project has one of those, so the looser test handed an installed IRA a
/// different data directory in every repository the user happened to be sitting
/// in -- and, on a first start there, 85 MB of models downloaded into somebody
/// else's project. Found by installing her and running `ira` from a checkout of
/// something else.
///
/// A malformed or unreadable manifest is not a checkout. Being wrong in that
/// direction sends her to the per-user directory, which always works; being
/// wrong in the other scatters her state across the disk.
fn in_iras_own_checkout() -> bool {
    let Ok(text) = std::fs::read_to_string("Cargo.toml") else {
        return false;
    };
    text.parse::<toml::Value>()
        .ok()
        .and_then(|manifest| {
            Some(manifest.get("package")?.get("name")?.as_str()?.to_string())
        })
        .is_some_and(|name| name == env!("CARGO_PKG_NAME"))
}

/// The per-user directory for this platform, by that platform's own convention.
///
/// No `directories` crate for this: three environment variables and a fallback.
/// The day a fourth platform needs an answer is the day it earns a dependency.
fn per_user() -> PathBuf {
    #[cfg(windows)]
    {
        // LOCALAPPDATA rather than APPDATA: models are a cache-sized download
        // and a roaming profile should not carry them between machines.
        if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|d| !d.is_empty()) {
            return PathBuf::from(local).join("IRA");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = home() {
            return home.join("Library").join("Application Support").join("IRA");
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(share) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
            return PathBuf::from(share).join("ira");
        }
        if let Some(home) = home() {
            return home.join(".local").join("share").join("ira");
        }
    }
    // No home directory at all. Rather than guess at a system location IRA may
    // not be able to write either, fall back to where she was started -- which
    // is exactly the old behaviour, and the old behaviour worked for years.
    PathBuf::from(".")
}

#[cfg(unix)]
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// Creates `data()` if it is not there, and says where it is.
///
/// Called once at start-up. A first run on a fresh machine has no directory at
/// all, and every writer below this point would otherwise fail one at a time
/// with its own half-explained error.
pub fn ensure() -> PathBuf {
    let dir = data();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::error!(path = %dir.display(), "could not create the data directory: {e}");
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `data()` reads the environment and the working directory, both of which
    /// are process-wide. Cargo runs tests in parallel, so they take turns.
    ///
    /// The same lock the database and settings tests take, not one of its own.
    /// `IRA_DATA` is a single global, and two mutexes guarding it serialise two
    /// disjoint sets of tests against each other while letting the two sets run
    /// at once -- which is no lock at all. It surfaced as a database test
    /// failing once in a run and passing on the retry.
    fn serially<T>(f: impl FnOnce() -> T) -> T {
        let _guard = crate::db::cwd_lock();
        f()
    }

    /// The rule the tests depend on: `IRA_DATA` beats everything, including a
    /// checkout. Without this, `cargo test` writes to the real per-user
    /// directory and edits the settings of the IRA the developer actually uses.
    #[test]
    fn ira_data_wins_even_in_a_checkout() {
        serially(|| {
            let want = std::env::temp_dir().join("ira-paths-test");
            std::env::set_var("IRA_DATA", &want);
            assert_eq!(data(), want);
            assert_eq!(in_data("models"), want.join("models"));
            std::env::remove_var("IRA_DATA");
        });
    }

    /// A checkout keeps behaving as it always did. This test file only exists
    /// inside one, so the working directory during `cargo test` is the case.
    #[test]
    fn a_checkout_stays_where_it_is() {
        serially(|| {
            std::env::remove_var("IRA_DATA");
            assert_eq!(data(), PathBuf::from("."), "cargo test runs in the checkout");
            assert!(in_iras_own_checkout());
        });
    }

    /// Somebody else's Rust project is not a checkout of this one.
    ///
    /// The regression this exists for: an installed IRA, run from any directory
    /// with a `Cargo.toml`, treated it as her own and would have downloaded
    /// 85 MB of models into it on a first start.
    #[test]
    fn another_projects_manifest_is_not_a_checkout() {
        serially(|| {
            let dir = std::env::temp_dir().join("ira-not-my-checkout");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("Cargo.toml"),
                b"[package]
name = \"totally-unrelated\"
version = \"0.1.0\"
",
            )
            .unwrap();

            let cwd = std::env::current_dir().unwrap();
            std::env::set_current_dir(&dir).unwrap();
            let verdict = in_iras_own_checkout();
            std::env::set_current_dir(cwd).unwrap();

            assert!(!verdict, "another project's manifest must not capture her");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// A manifest that will not parse is not a checkout either. Falling through
    /// to the per-user directory always works; guessing "checkout" does not.
    #[test]
    fn a_broken_manifest_is_not_a_checkout() {
        serially(|| {
            let dir = std::env::temp_dir().join("ira-broken-manifest");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("Cargo.toml"), b"[package
name = ira").unwrap();

            let cwd = std::env::current_dir().unwrap();
            std::env::set_current_dir(&dir).unwrap();
            let verdict = in_iras_own_checkout();
            std::env::set_current_dir(cwd).unwrap();

            assert!(!verdict);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// An empty `IRA_DATA` is an unset one. Shells set variables to nothing all
    /// the time, and `PathBuf::from("")` joins into a relative path that writes
    /// the database into whatever directory IRA happened to start in.
    #[test]
    fn an_empty_setting_is_not_a_path() {
        serially(|| {
            std::env::set_var("IRA_DATA", "");
            assert_eq!(data(), PathBuf::from("."), "falls through to the checkout");
            std::env::remove_var("IRA_DATA");
        });
    }
}
