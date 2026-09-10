//! `ira doctor` -- say what is wrong before the microphone is open.
//!
//! Anything knowable at boot must fail at boot. A missing key that surfaces as
//! silence three seconds into the first conversation is the worst version of
//! the same error: by then the user is talking, and IRA looks broken rather
//! than unconfigured.
//!
//! The fatal subset runs on every start-up, not just under `doctor`.

use std::path::{Path, PathBuf};

/// What to run when something is missing.
///
/// It used to name a platform's shell script, which is right for a checkout
/// and useless anywhere else: an installed IRA has no `scripts/` directory
/// beside it, so the remedy named a file the person reading it did not have.
/// `ira fetch` is the same download, and it is on the machine by definition --
/// it is the program printing the message.
pub const SETUP: &str = "ira fetch";

pub enum Level {
    Ok,
    /// Works, but something will be missing or slower than it needs to be.
    Warn,
    /// Refuse to start.
    Fatal,
}

pub struct Check {
    pub level: Level,
    pub what: String,
    /// What to do about it. Empty when there is nothing to do.
    pub fix: String,
}

impl Check {
    fn ok(what: impl Into<String>) -> Self {
        Self { level: Level::Ok, what: what.into(), fix: String::new() }
    }
    fn warn(what: impl Into<String>, fix: impl Into<String>) -> Self {
        Self { level: Level::Warn, what: what.into(), fix: fix.into() }
    }
    fn fatal(what: impl Into<String>, fix: impl Into<String>) -> Self {
        Self { level: Level::Fatal, what: what.into(), fix: fix.into() }
    }
}

/// The API keys this configuration actually needs.
///
/// Pure so the four combinations can be tested without mutating the process
/// environment, which races every other test in the binary.
pub fn required_keys(stt_url: Option<&str>, llm_url: Option<&str>) -> Vec<&'static str> {
    let mut keys = Vec::new();
    if stt_url.is_none() {
        keys.push("GROQ_API_KEY");
    }
    if llm_url.is_none() {
        keys.push("ANTHROPIC_API_KEY");
    }
    keys
}

pub struct Paths {
    pub models: PathBuf,
    pub wakeword: String,
    pub voice: PathBuf,
    pub piper: PathBuf,
}

/// Everything checkable without opening the microphone.
pub fn files_and_keys(p: &Paths) -> Vec<Check> {
    let mut out = Vec::new();

    let mut need = |name: &str, path: PathBuf| {
        if path.is_file() {
            out.push(Check::ok(format!("{name} present")));
        } else {
            out.push(Check::fatal(
                format!("{name} missing at {}", path.display()),
                format!("run {SETUP}"),
            ));
        }
    };

    need("melspectrogram", p.models.join("melspectrogram.onnx"));
    need("embedding model", p.models.join("embedding_model.onnx"));
    need("wake classifier", p.models.join(&p.wakeword));
    need("silero vad", p.models.join("silero_vad.onnx"));
    need("voice", p.voice.clone());
    // Piper reads the sample rate from this sidecar; without it every reply
    // plays at the wrong pitch rather than failing outright.
    need("voice config", p.voice.with_extension("onnx.json"));

    if p.piper.is_file() {
        out.push(Check::ok("piper present"));
    } else {
        out.push(Check::fatal(
            format!("piper missing at {}", p.piper.display()),
            format!("run {SETUP}, or set IRA_PIPER"),
        ));
    }

    let stt_url = crate::settings::get("IRA_STT_URL");
    let llm_url = crate::settings::get("IRA_LLM_URL");

    for key in required_keys(stt_url.as_deref(), llm_url.as_deref()) {
        if crate::settings::is_set(key) {
            out.push(Check::ok(format!("{key} set")));
        } else {
            out.push(Check::fatal(
                format!("{key} not set"),
                match key {
                    "GROQ_API_KEY" => "run `ira set GROQ_API_KEY <key>`, or `ira set IRA_STT_URL <url>` for local STT",
                    _ => "run `ira set ANTHROPIC_API_KEY <key>`, or `ira set IRA_LLM_URL <url>` for another provider",
                },
            ));
        }
    }

    if stt_url.is_some() {
        out.push(Check::ok("STT local -- no audio leaves this machine"));
    }

    // Every gateway names models differently, so the Anthropic default is
    // almost certainly wrong on someone else's endpoint.
    if llm_url.is_some() && !crate::settings::is_set("IRA_LLM_MODEL") {
        out.push(Check::warn(
            "IRA_LLM_MODEL not set while IRA_LLM_URL is",
            "gateways use their own model ids, e.g. anthropic/claude-sonnet-4.5",
        ));
    }

    out
}

/// Runs every check, including the ones that need the network and the mic.
pub async fn all(p: &Paths) -> Vec<Check> {
    let mut out = files_and_keys(p);

    match input_device() {
        Some(name) => out.push(Check::ok(format!("microphone: {name}"))),
        None => out.push(Check::fatal(
            "no default input device",
            "plug in a microphone, or check OS input permissions",
        )),
    }

    if let Some(url) = crate::settings::get("IRA_STT_URL") {
        let client = reqwest::Client::new();
        let probe = client
            .get(&url)
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await;
        // Any HTTP answer means something is listening. whisper-server has no
        // health endpoint, so "it replied at all" is the whole test.
        match probe {
            Ok(_) => out.push(Check::ok(format!("STT reachable at {url}"))),
            Err(e) => out.push(Check::fatal(
                format!("STT unreachable at {url}: {e}"),
                "start whisper-server, or `ira set IRA_STT_URL` to use Groq",
            )),
        }
    }

    out
}

fn input_device() -> Option<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let device = cpal::default_host().default_input_device()?;
    // A device that cannot report a config cannot be opened either.
    device.default_input_config().ok()?;
    Some(
        device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "unnamed".into()),
    )
}

/// Prints the report. Returns the process exit code.
pub fn report(checks: &[Check]) -> i32 {
    let mut fatal = 0;
    for c in checks {
        let (mark, tail) = match c.level {
            Level::Ok => ("ok  ", String::new()),
            Level::Warn => ("warn", format!("  -- {}", c.fix)),
            Level::Fatal => {
                fatal += 1;
                ("FAIL", format!("  -- {}", c.fix))
            }
        };
        println!("{mark}  {}{tail}", c.what);
    }
    println!();
    if fatal == 0 {
        println!("ready");
        0
    } else {
        println!("{fatal} problem(s) to fix before IRA will start");
        1
    }
}

/// The boot gate: the first fatal problem, formatted for an error message.
pub fn first_fatal(checks: &[Check]) -> Option<String> {
    checks.iter().find_map(|c| match c.level {
        Level::Fatal if c.fix.is_empty() => Some(c.what.clone()),
        Level::Fatal => Some(format!("{} -- {}", c.what, c.fix)),
        _ => None,
    })
}

pub fn paths(models: &Path, wakeword: &str, voice: &Path, piper: &Path) -> Paths {
    Paths {
        models: models.to_path_buf(),
        wakeword: wakeword.to_string(),
        voice: voice.to_path_buf(),
        piper: piper.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each backend swap must retire exactly the key it replaces. Getting this
    /// wrong demands a key nobody needs, which is a refusal to start over
    /// nothing.
    #[test]
    fn each_url_retires_its_own_key() {
        assert_eq!(
            required_keys(None, None),
            vec!["GROQ_API_KEY", "ANTHROPIC_API_KEY"]
        );
        assert_eq!(required_keys(Some("http://x/inference"), None), vec!["ANTHROPIC_API_KEY"]);
        assert_eq!(required_keys(None, Some("http://x/v1/chat")), vec!["GROQ_API_KEY"]);
        assert!(required_keys(Some("http://x"), Some("http://y")).is_empty());
    }

    #[test]
    fn a_missing_model_is_fatal_and_names_the_fix() {
        let p = paths(
            Path::new("no-such-dir"),
            "nope.onnx",
            Path::new("no-such-dir/voice.onnx"),
            Path::new("no-such-dir/piper.exe"),
        );
        let checks = files_and_keys(&p);
        let fatal = first_fatal(&checks).expect("missing models must be fatal");
        // The fix must name a script that exists on the platform being run on.
        assert!(fatal.contains(SETUP), "no usable fix offered: {fatal}");
    }
}
