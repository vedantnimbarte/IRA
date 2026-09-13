//! Local speech-to-text: a whisper.cpp server IRA starts, watches and stops.
//!
//! [decisions/0020](../docs/decisions/0020-she-runs-whisper-herself.md). Lifted
//! from Echo's `whisper_server.rs` and `local.rs`, minus what a voice loop does
//! not need (partials, dictionary prompts, per-app profiles).
//!
//! ```text
//!   whisper/                 the pack `ira fetch --whisper` chose (CUDA or CPU)
//!   whisper/cpu/             the CPU pack, only beside a CUDA one
//!   models/ggml-<model>.bin  the weights
//! ```
//!
//! Three ways down, each one step slower and each logged:
//!
//! - **Server, resident.** The model loads once; a turn costs only the decode.
//! - **CPU instead of CUDA.** The first time a CUDA server fails, the rest of
//!   the session uses the CPU pack. A driver that failed once fails again.
//! - **whisper-cli.** When no server will run at all. Reloads the model every
//!   turn, which is slow and still better than "I didn't catch that".
//!
//! A server that has died is noticed on the next turn and started again, and a
//! change of model restarts it, because the running one is compared against
//! what the settings ask for before every request.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// The picker's list. English-only first: IRA's prompts, voice and wake word
/// are English, and `.en` is both smaller and more accurate for English.
/// `medium` and up are left out -- too slow to talk to on the hardware this is
/// measured on (docs/ROADMAP.md).
pub const MODELS: &[&str] = &["tiny.en", "base.en", "small.en", "tiny", "base", "small"];

/// Reading the weights from disk comes before listening, so this covers a cold
/// page cache. A CUDA server also uploads them and compiles kernels on its first
/// run, which some drivers take minutes over.
const CPU_STARTUP: Duration = Duration::from_secs(30);
const GPU_STARTUP: Duration = Duration::from_secs(120);

static MODELS_DIR: OnceLock<PathBuf> = OnceLock::new();
static GPU_FAILED: AtomicBool = AtomicBool::new(false);
static SERVER: tokio::sync::Mutex<Option<Running>> = tokio::sync::Mutex::const_new(None);

struct Running {
    child: Child,
    port: u16,
    binary: PathBuf,
    model: PathBuf,
}

/// Where the weights are, which `main` resolves from `IRA_MODELS`. Everything
/// else here is found relative to it, exactly as `ira fetch` lays it out.
pub fn init(models: &Path) {
    let _ = MODELS_DIR.set(models.to_path_buf());
}

fn models_dir() -> PathBuf {
    MODELS_DIR.get().cloned().unwrap_or_else(|| crate::paths::in_data("models"))
}

/// The binaries' directory: beside `models/`, where `ira fetch` has always put it.
pub fn dir() -> PathBuf {
    models_dir().parent().unwrap_or(Path::new(".")).join("whisper")
}

pub fn exe(dir: &Path, name: &str) -> PathBuf {
    dir.join(if cfg!(windows) { format!("{name}.exe") } else { name.to_string() })
}

/// Whether a pack was built against CUDA. The cuBLAS archives carry this DLL
/// and the CPU one does not; a source build on unix links it statically, so
/// there it reads as CPU and simply has no CPU pack to fall back to.
pub fn is_cuda_pack(dir: &Path) -> bool {
    dir.join("ggml-cuda.dll").is_file()
}

/// The pack to run now: the CPU one once CUDA has failed, if there is one.
fn active_dir() -> PathBuf {
    let root = dir();
    let cpu = root.join("cpu");
    if GPU_FAILED.load(Ordering::Relaxed) && exe(&cpu, "whisper-server").is_file() {
        cpu
    } else {
        root
    }
}

/// The model to use: the one picked, or one sized for this machine.
///
/// A CUDA pack affords small.en, which keeps proper nouns tiny.en drops. On CPU
/// small.en is too slow to talk to.
///
/// ponytail: decided once per start. After a CUDA failure the CPU pack keeps
/// running small.en, slowly; pick tiny.en in settings if that happens.
pub fn model() -> String {
    crate::settings::get("IRA_WHISPER_MODEL").unwrap_or_else(|| auto_model().to_string())
}

pub fn auto_model() -> &'static str {
    static AUTO: OnceLock<&'static str> = OnceLock::new();
    AUTO.get_or_init(|| {
        let root = dir();
        let gpu = if exe(&root, "whisper-server").is_file() {
            is_cuda_pack(&root)
        } else {
            crate::fetch::detect_backend() != "cpu"
        };
        if gpu { "small.en" } else { "tiny.en" }
    })
}

pub fn model_path() -> PathBuf {
    models_dir().join(format!("ggml-{}.bin", model()))
}

/// Everything a local turn needs is on disk.
pub fn installed() -> bool {
    exe(&dir(), "whisper-server").is_file() && model_path().is_file()
}

/// Installed, and with the CPU pack a CUDA one falls back to. A CUDA pack from
/// before the fallback existed has none, and this is what fetches it.
pub fn complete() -> bool {
    installed() && (!is_cuda_pack(&dir()) || exe(&dir().join("cpu"), "whisper-server").is_file())
}

/// Transcribes one WAV, by whichever of the three routes still works.
pub async fn transcribe(wav: Vec<u8>) -> Result<String> {
    let model = model_path();
    if !model.is_file() {
        bail!("no whisper model at {} -- run `ira fetch --whisper`", model.display());
    }
    match server(&wav, &model).await {
        Ok(text) => Ok(text),
        Err(e) => {
            if is_cuda_pack(&active_dir()) {
                tracing::warn!("CUDA whisper-server failed; CPU for the rest of this session");
                GPU_FAILED.store(true, Ordering::Relaxed);
            }
            tracing::warn!("whisper-server failed, falling back to whisper-cli: {e:#}");
            cli(&wav, &model).await
        }
    }
}

/// Starts the server ahead of the first turn, so its model load is not what the
/// first question waits on -- a CUDA start alone can outlast the turn's timeout.
pub async fn warm() {
    if !model_path().is_file() {
        return;
    }
    let model = model_path();
    if let Err(e) = ensure(&model).await {
        tracing::warn!("whisper-server did not start: {e:#}");
        if is_cuda_pack(&active_dir()) {
            GPU_FAILED.store(true, Ordering::Relaxed);
            if let Err(e) = ensure(&model).await {
                tracing::warn!("CPU whisper-server did not start either: {e:#}");
            }
        }
    }
}

/// Stops the server. On a normal exit, and when the engine is switched away
/// from it -- an idle one holds the model in memory for nothing.
pub async fn stop() {
    if let Some(mut r) = SERVER.lock().await.take() {
        let _ = r.child.kill().await;
        tracing::info!("whisper-server stopped");
    }
}

async fn server(wav: &[u8], model: &Path) -> Result<String> {
    let port = ensure(model).await?;
    let form = reqwest::multipart::Form::new()
        .part(
            "file",
            reqwest::multipart::Part::bytes(wav.to_vec())
                .file_name("turn.wav")
                .mime_str("audio/wav")?,
        )
        .text("response_format", "json")
        // whisper-server defaults to English, so a multilingual model has to be
        // told to listen for anything.
        .text("language", language());
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/inference"))
        // A turn is seconds of speech. A server still busy after this is wedged.
        .timeout(Duration::from_secs(30))
        .multipart(form)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("whisper-server {}: {}", resp.status(), resp.text().await?);
    }
    let v: serde_json::Value = resp.json().await?;
    Ok(clean(v["text"].as_str().unwrap_or_default()))
}

/// The port of a server running what the settings ask for, starting one if it
/// is not -- because it never was, it died, or the model has changed since.
async fn ensure(model: &Path) -> Result<u16> {
    let binary = exe(&active_dir(), "whisper-server");
    let mut guard = SERVER.lock().await;
    if let Some(r) = guard.as_mut() {
        // `try_wait` is what tells a crashed server from a running one; the
        // struct left behind looks perfectly healthy either way.
        let alive = matches!(r.child.try_wait(), Ok(None));
        if alive && r.binary == binary && r.model == model {
            return Ok(r.port);
        }
        if !alive {
            tracing::warn!("whisper-server had exited; starting it again");
        }
        let _ = r.child.kill().await;
        *guard = None;
    }
    let r = start(&binary, model).await?;
    let port = r.port;
    *guard = Some(r);
    Ok(port)
}

async fn start(binary: &Path, model: &Path) -> Result<Running> {
    if !binary.is_file() {
        bail!("no whisper-server at {} -- run `ira fetch --whisper`", binary.display());
    }
    // Asked of the OS rather than fixed, so it cannot collide with whatever else
    // is running. Released before the server binds it: losing that race makes
    // the server exit at once, which the wait below sees immediately.
    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();

    let mut cmd = Command::new(binary);
    cmd.arg("-m")
        .arg(model)
        .args(["--host", "127.0.0.1", "--port", &port.to_string()])
        .args(["-t", &threads().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = cmd
        .spawn()
        .with_context(|| format!("start {}", binary.display()))?;
    #[cfg(windows)]
    job::contain(&child);

    // Drained continuously, or a full pipe stalls the server. Kept, capped, so
    // a failed start can say why.
    let stderr = Arc::new(Mutex::new(String::new()));
    if let Some(pipe) = child.stderr.take() {
        let sink = stderr.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(pipe).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(mut s) = sink.lock() {
                    if s.len() < 4096 {
                        s.push_str(&line);
                        s.push('\n');
                    }
                }
            }
        });
    }

    let gpu = is_cuda_pack(binary.parent().unwrap_or(Path::new(".")));
    let wait = if gpu { GPU_STARTUP } else { CPU_STARTUP };
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        // Checked first: a server that has exited will never bind, and waiting
        // out the whole timeout for it delays the CPU fallback for nothing.
        if let Ok(Some(status)) = child.try_wait() {
            let why = stderr.lock().map(|s| s.trim().to_string()).unwrap_or_default();
            bail!("whisper-server exited during start-up ({status}): {why}");
        }
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill().await;
            bail!("whisper-server did not start within {}s", wait.as_secs());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tracing::info!(port, gpu, model = %model.display(), "whisper-server ready");
    Ok(Running { child, port, binary: binary.to_path_buf(), model: model.to_path_buf() })
}

/// The one-shot CLI: no port, no process to keep alive, and a model load per
/// turn. What is left when the server will not run.
async fn cli(wav: &[u8], model: &Path) -> Result<String> {
    let binary = exe(&active_dir(), "whisper-cli");
    if !binary.is_file() {
        bail!("no whisper-cli at {}", binary.display());
    }
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "ira-{}-{}.wav",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    tokio::fs::write(&tmp, wav).await?;

    let mut cmd = Command::new(&binary);
    cmd.arg("-m")
        .arg(model)
        .arg("-f")
        .arg(&tmp)
        .args(["-l", language(), "-t", &threads().to_string()])
        .args(["-nt", "-np"]) // plain text on stdout, nothing else
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = cmd.output().await;
    let _ = tokio::fs::remove_file(&tmp).await;
    let out = out.with_context(|| format!("run {}", binary.display()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "whisper-cli {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(clean(&String::from_utf8_lossy(&out.stdout)))
}

/// English-only models are told so; the rest detect it.
fn language() -> &'static str {
    if model().ends_with(".en") { "en" } else { "auto" }
}

/// Measured at 8 on the reference machine (ROADMAP); more cores than that stop
/// helping a model this size.
fn threads() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get().min(8))
}

/// One line, without whisper.cpp's own marker for silence. The marker comes
/// from its front-end rather than the decoder, so it reaches the text even when
/// nothing was said -- and IRA would answer "[BLANK_AUDIO]".
pub fn clean(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "[BLANK_AUDIO]")
        .collect::<Vec<_>>()
        .join(" ")
}

/// What the settings window says under the engine switch, as (still working,
/// fine, words): what is downloading, what failed, or what is ready.
pub fn status() -> (bool, bool, String) {
    if !crate::stt::local() {
        let also = if installed() { " Falls back to this machine if Groq fails." } else { "" };
        return (false, true, format!("Transcribing at Groq.{also}"));
    }
    if !crate::stt::managed() {
        return (false, true, "Using your own whisper-server. No audio leaves this machine.".into());
    }
    if DOWNLOADING.load(Ordering::Relaxed) {
        let now = crate::fetch::progress_line();
        return (true, false, if now.is_empty() { "Downloading…".into() } else { now });
    }
    if let Some(e) = LAST_ERROR.lock().ok().and_then(|e| e.clone()) {
        return (false, false, e);
    }
    if !exe(&dir(), "whisper-server").is_file() {
        return (false, false, "whisper-server is not installed.".into());
    }
    if !model_path().is_file() {
        return (false, false, format!("{} is not downloaded.", model()));
    }
    let on = if GPU_FAILED.load(Ordering::Relaxed) || !is_cuda_pack(&dir()) { "CPU" } else { "GPU" };
    (false, true, format!("Ready: {} on the {on}. No audio leaves this machine.", model()))
}

static DOWNLOADING: AtomicBool = AtomicBool::new(false);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Brings the running IRA in line with the engine settings after one changes:
/// downloads what local needs and starts it, or stops a server nobody uses.
pub fn apply() {
    // Claimed before returning, so the page's reload straight after a save
    // already sees the download and starts watching it.
    let fetch = crate::stt::managed() && !complete() && !DOWNLOADING.swap(true, Ordering::Relaxed);
    if let Ok(mut e) = LAST_ERROR.lock() {
        *e = None;
    }
    tokio::spawn(async move {
        if !crate::stt::managed() {
            stop().await;
            return;
        }
        if fetch {
            let got = crate::fetch::whisper_files(&models_dir(), None).await;
            DOWNLOADING.store(false, Ordering::Relaxed);
            let failure = match got {
                Err(e) => Some(format!("Download failed: {e:#}")),
                // Unix gets the model but no binary, and instructions instead.
                Ok(()) if !installed() => Some(
                    "whisper.cpp has no prebuilt binary here. `ira fetch --whisper` prints the build commands.".into(),
                ),
                Ok(()) => None,
            };
            if let Some(f) = failure {
                tracing::error!("{f}");
                if let Ok(mut e) = LAST_ERROR.lock() {
                    *e = Some(f);
                }
                return;
            }
        }
        // Also the model-change path: `ensure` sees the new model and restarts.
        if installed() {
            warm().await;
        }
    });
}

/// Ties whisper-server's life to IRA's on Windows, however IRA ends.
///
/// `kill_on_drop` only runs if the `Child` is dropped, and a static never is --
/// nor is anything when the process is ended from Task Manager. A job object
/// with kill-on-close is released by the kernel whatever the exit, and takes
/// the server with it.
///
/// ponytail: unix has no equivalent short of `prctl`, so a killed IRA there
/// leaves the server running until it is killed too. A normal exit stops it.
#[cfg(windows)]
mod job {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub fn contain(child: &tokio::process::Child) {
        static JOB: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let job = *JOB.get_or_init(|| unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            job as usize
        });
        if let Some(handle) = child.raw_handle() {
            if unsafe { AssignProcessToJobObject(job as _, handle as _) } == 0 {
                tracing::warn!("whisper-server could not be tied to IRA's lifetime");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All three routes, against the real binaries and the corpus. Skips unless
    /// `ira fetch --whisper` has filled the data directory `IRA_DATA` names:
    ///   IRA_DATA=<dir> cargo test every_route -- --nocapture
    #[tokio::test]
    async fn every_route_hears_the_corpus() {
        // Settings are not loaded in a test, so use whichever model was fetched.
        if let Some(m) = MODELS.iter().find(|m| models_dir().join(format!("ggml-{m}.bin")).is_file()) {
            crate::settings::set_in_memory("IRA_WHISPER_MODEL", m);
        }
        if !installed() {
            eprintln!("skipped: no whisper in the data directory; run `ira fetch --whisper`");
            return;
        }
        let wav = std::fs::read("corpus/speech-16k.wav").unwrap();
        let model = model_path();

        // The pack fetch chose, or the CLI if it will not run here.
        let served = transcribe(wav.clone()).await.unwrap();
        let pack_ran = !GPU_FAILED.load(Ordering::Relaxed);
        // After a CUDA failure: the CPU pack's server, where there is one.
        GPU_FAILED.store(true, Ordering::Relaxed);
        let cpu = server(&wav, &model).await.unwrap();
        let once = cli(&wav, &model).await.unwrap();
        stop().await;

        eprintln!("first ({}): {served}\ncpu server: {cpu}\ncli:        {once}",
            if pack_ran { "fetched pack's server" } else { "fell back" });
        for text in [served, cpu, once] {
            assert!(text.split_whitespace().count() > 2, "heard next to nothing: {text:?}");
        }
    }

    #[test]
    fn silence_is_not_something_to_answer() {
        assert_eq!(clean("[BLANK_AUDIO]\n"), "");
        assert_eq!(clean(" hello there \n[BLANK_AUDIO]\n world \n"), "hello there world");
    }
}
