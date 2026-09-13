//! `ira fetch`: the weights, the voice and piper, downloaded into the data
//! directory.
//!
//! `scripts/fetch-models.ps1` and `.sh` did this, and still do -- they are what
//! CI runs and what a checkout uses. This is the same list of pinned URLs for
//! the case they cannot serve: an installed IRA has no `scripts/` directory
//! next to it, and telling someone who ran an installer to go and find a shell
//! script is telling them the install did not finish.
//!
//! It also runs itself. A first start with no models does not fail a preflight
//! check and stop -- it fetches, says so, and carries on into the first
//! conversation. Only if that download *fails* does the old fatal check apply,
//! and then the remedy it names is `ira fetch`, which is a command the person
//! reading it already has.
//!
//! **Extraction shells out to `tar`.** Windows 10 and later ship bsdtar as
//! `tar.exe` and it reads zip; every unix has had `tar` forever. That is one
//! `Command` against three crates (`zip`, `flate2`, `tar`) and their trees, for
//! a step that runs at most twice in this program's life.
//!
//! **Whisper is not fully here.** On Windows it is a prebuilt archive, so
//! `--whisper` fetches it. On Linux and macOS upstream publishes no binaries
//! and the script builds from source with cmake -- driving a git clone and a
//! C++ build from inside a voice assistant is not a thing this file will do, so
//! there it prints the three commands instead.

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Pinned, exactly as the scripts pin them. An unpinned "latest" is a build
/// that works until upstream tags something, and then does not.
const OWW: &str = "https://github.com/dscripka/openWakeWord/releases/download/v0.5.1";
const SILERO: &str = "https://raw.githubusercontent.com/snakers4/silero-vad/master/src/silero_vad/data/silero_vad.onnx";
const VOICES: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/amy/medium";
const PIPER_RELEASE: &str = "https://github.com/rhasspy/piper/releases/download/2023.11.14-2";
const WHISPER_RELEASE: &str = "https://github.com/ggml-org/whisper.cpp/releases/download/v1.7.6";
const GGML: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/// The files that must exist before IRA can hear or speak, and where each comes
/// from. `IRA_WAKEWORD` and `IRA_VOICE` can name others; this fetches the
/// defaults, which is what a first run needs and what the checks look for.
fn core(models: &Path) -> Vec<(String, PathBuf)> {
    vec![
        (format!("{OWW}/melspectrogram.onnx"), models.join("melspectrogram.onnx")),
        (format!("{OWW}/embedding_model.onnx"), models.join("embedding_model.onnx")),
        (format!("{OWW}/hey_jarvis_v0.1.onnx"), models.join("hey_jarvis_v0.1.onnx")),
        (SILERO.to_string(), models.join("silero_vad.onnx")),
        (format!("{VOICES}/en_US-amy-medium.onnx"), models.join("en_US-amy-medium.onnx")),
        (format!("{VOICES}/en_US-amy-medium.onnx.json"), models.join("en_US-amy-medium.onnx.json")),
    ]
}

/// The piper archive for this platform, or `None` where upstream publishes none.
fn piper_asset() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "piper_windows_amd64.zip",
        ("linux", "x86_64") => "piper_linux_x86_64.tar.gz",
        ("linux", "aarch64") => "piper_linux_aarch64.tar.gz",
        ("macos", "x86_64") => "piper_macos_x64.tar.gz",
        ("macos", "aarch64") => "piper_macos_aarch64.tar.gz",
        _ => return None,
    })
}

/// True when everything `core` fetches, plus piper, is already on disk.
///
/// The voice is checked by the path IRA will actually load rather than by the
/// default name, so someone who set `IRA_VOICE` to a voice they installed
/// themselves is not told to download one they already have.
pub fn have_everything(models: &Path, voice: &Path, piper: &Path) -> bool {
    voice.is_file()
        && piper.is_file()
        && core(models).iter().all(|(_, dest)| dest.is_file())
}

/// Downloads whatever is missing. Present files are left alone, so this is safe
/// to re-run after a failure part-way through -- which is the whole point of it
/// being a command as well as a first-run step.
pub async fn core_files(models: &Path, piper: &Path) -> Result<()> {
    std::fs::create_dir_all(models)
        .with_context(|| format!("create {}", models.display()))?;
    let client = reqwest::Client::new();

    for (url, dest) in core(models) {
        get(&client, &url, &dest).await?;
    }

    if piper.is_file() {
        eprintln!("have  {}", name_of(piper));
        return Ok(());
    }
    let asset = piper_asset().ok_or_else(|| {
        anyhow!(
            "no prebuilt piper for {}-{}; build one and set IRA_PIPER to it",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    // The archive holds a `piper/` directory, so it unpacks into the parent of
    // the executable's own directory and lands exactly where IRA looks.
    let into = piper
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| anyhow!("IRA_PIPER has no directory to unpack beside: {}", piper.display()))?;
    unpack(&client, &format!("{PIPER_RELEASE}/{asset}"), into, None).await?;

    // Check the one file that matters rather than trusting the extraction: an
    // archive can unpack partially and still exit zero, and the symptom of that
    // is a piper that will not start three seconds into the first sentence.
    if !piper.is_file() {
        bail!(
            "{} missing after unpacking {asset} -- delete {} and run `ira fetch` again",
            name_of(piper),
            into.join("piper").display()
        );
    }
    Ok(())
}

/// `ira fetch`. Returns the exit code.
pub async fn run(args: &[String], models: &Path, voice: &Path, piper: &Path) -> i32 {
    let mut whisper = false;
    let mut model: Option<String> = None;
    let mut backend: Option<String> = None;
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--whisper" => whisper = true,
            "--model" => model = rest.next().cloned(),
            "--backend" => backend = rest.next().cloned(),
            "-h" | "--help" => {
                eprintln!("usage: ira fetch [--whisper] [--model <name>] [--backend cpu|cuda11|cuda12]");
                eprintln!();
                eprintln!("Downloads the wake models, the VAD, the voice and piper into");
                eprintln!("the data directory. Files already there are left alone.");
                eprintln!();
                eprintln!("--whisper adds local speech-to-text, which IRA also fetches by");
                eprintln!("herself when it is the engine and not yet here. --model picks");
                eprintln!("the model and saves the choice: {}.", crate::whisper::MODELS.join(", "));
                return 0;
            }
            other => {
                eprintln!("unknown option: {other}");
                return 2;
            }
        }
    }
    // Saved rather than only fetched: a model on disk that the settings do not
    // name is 466 MB IRA never uses.
    if let Some(m) = &model {
        if let Err(e) = crate::settings::set("IRA_WHISPER_MODEL", m) {
            eprintln!("{e:#}");
            return 2;
        }
    }

    if have_everything(models, voice, piper) && !whisper {
        eprintln!("everything is already here: {}", models.display());
        return 0;
    }

    if let Err(e) = core_files(models, piper).await {
        eprintln!("fetch failed: {e:#}");
        return 1;
    }
    if whisper {
        if let Err(e) = whisper_files(models, backend).await {
            eprintln!("fetch failed: {e:#}");
            return 1;
        }
    }
    eprintln!();
    eprintln!("done. now:");
    eprintln!("  ira set ANTHROPIC_API_KEY sk-ant-...");
    eprintln!("  ira");
    0
}

/// SHA-256 of each whisper.cpp v1.7.6 archive, lowercase hex.
///
/// What comes out of these is an executable IRA then runs, so the archive is
/// checked before it is unpacked. Recorded from the GitHub release API's own
/// `digest` field -- the same values Echo pins. Bumping the release means
/// re-recording them from `api.github.com/repos/ggml-org/whisper.cpp/releases/tags/<tag>`.
fn whisper_asset(backend: &str) -> Result<(&'static str, &'static str)> {
    Ok(match backend {
        "cpu" => ("whisper-bin-x64.zip", "0d2eca299c248f965bd0341bcb219db4b433c7f0c0ce2200d4df85765e8156a9"),
        "cuda11" => ("whisper-cublas-11.8.0-bin-x64.zip", "d42f531781627f8cdceffc18fa03414ae90d1748a5c3f103ada64c991dd7f828"),
        "cuda12" => ("whisper-cublas-12.4.0-bin-x64.zip", "3fc4d3ebd9a678313de50c04d9e59c43117ae190f0cb7bff602d4aeefc4efe3d"),
        other => bail!("unknown backend {other}; expected cpu, cuda11 or cuda12"),
    })
}

/// Local speech-to-text: the binaries, and the model the settings name.
///
/// Windows gets the prebuilt archives upstream publishes -- the one for this
/// machine's GPU in `whisper/`, and beside a CUDA one the CPU pack in
/// `whisper/cpu/`, which is what IRA falls back to when CUDA fails. Separate
/// directories because each ships its own conflicting copy of the ggml DLLs.
///
/// Nothing else has prebuilt binaries, and the source build the shell script
/// does -- clone, cmake, copy -- is not something to run from inside a running
/// assistant, so those platforms get the commands.
///
/// Also what `whisper::apply` runs when the window switches to local.
pub async fn whisper_files(models: &Path, backend: Option<String>) -> Result<()> {
    let client = reqwest::Client::new();
    let dir = crate::whisper::dir();
    let model = crate::whisper::model();

    if !cfg!(windows) {
        if !crate::whisper::exe(&dir, "whisper-server").is_file() {
            eprintln!();
            eprintln!("whisper.cpp publishes no prebuilt binaries for {}.", std::env::consts::OS);
            eprintln!("It is a source build, and it is tuned for the machine that builds it:");
            eprintln!();
            eprintln!("  git clone --depth 1 --branch v1.7.6 https://github.com/ggml-org/whisper.cpp w");
            eprintln!("  cmake -S w -B w/build -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF");
            eprintln!("  cmake --build w/build --config Release -j");
            eprintln!("  mkdir -p {} && cp w/build/bin/whisper-* {}", dir.display(), dir.display());
            eprintln!();
            eprintln!("Or transcribe at Groq instead: ira set IRA_STT_ENGINE cloud");
            eprintln!();
            eprintln!("The model itself is a download, and that part is done here.");
        }
        get(&client, &format!("{GGML}/ggml-{model}.bin"), &models.join(format!("ggml-{model}.bin"))).await?;
        return Ok(());
    }

    let backend = match backend {
        Some(b) => b,
        None => {
            let b = detect_backend();
            eprintln!("gpu   {}", if b == "cpu" { "none detected -- CPU build".to_string() } else { format!("NVIDIA -- {b} build") });
            b
        }
    };
    pack(&client, &dir, &backend).await?;
    if crate::whisper::is_cuda_pack(&dir) {
        pack(&client, &dir.join("cpu"), "cpu").await?;
    }
    get(&client, &format!("{GGML}/ggml-{model}.bin"), &models.join(format!("ggml-{model}.bin"))).await?;
    Ok(())
}

/// One whisper.cpp archive, verified and flattened into `dir`, unless its
/// server is already there.
async fn pack(client: &reqwest::Client, dir: &Path, backend: &str) -> Result<()> {
    let server = crate::whisper::exe(dir, "whisper-server");
    if server.is_file() {
        eprintln!("have  {}", server.display());
        return Ok(());
    }
    let (asset, sha) = whisper_asset(backend)?;
    let stage = dir.with_extension("unpacking");
    let _ = std::fs::remove_dir_all(&stage);
    unpack(client, &format!("{WHISPER_RELEASE}/{asset}"), &stage, Some(sha)).await?;
    // The archive nests everything under `Release\`. Flatten it: the
    // binaries load their ggml and cudart DLLs from their own directory, so
    // a preserved folder layout gives you an exe that cannot start.
    std::fs::create_dir_all(dir)?;
    flatten(&stage, dir)?;
    let _ = std::fs::remove_dir_all(&stage);
    if !server.is_file() {
        bail!(
            "whisper-server.exe missing after unpacking {asset} -- delete {} and try again",
            dir.display()
        );
    }
    Ok(())
}

/// Which CUDA generation the *driver* supports, as a whisper backend name.
///
/// nvidia-smi ships with every NVIDIA driver, so its absence is a reliable "no
/// card here" rather than something to warn about.
///
/// cuda12 whenever the driver can run it, despite 443 MB against cuda11's 45:
/// the 11.8 archive leaves cuBLAS out, so on a machine without the CUDA 11
/// toolkit installed its server exits at once with a missing DLL. The 12.4
/// archive carries cuBLAS itself. Found by running the 11.8 pack on a GTX 1650.
pub fn detect_backend() -> String {
    match std::process::Command::new("nvidia-smi").output() {
        Ok(out) if out.status.success() => backend_for(&String::from_utf8_lossy(&out.stdout)).into(),
        _ => "cpu".into(),
    }
}

/// The pack for what `nvidia-smi` printed. Its header reads "CUDA Version: 13.1",
/// the newest generation the driver runs, and older ones stay compatible.
fn backend_for(smi: &str) -> &'static str {
    let major = smi
        .split("CUDA Version:")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .and_then(|v| v.split('.').next()?.parse::<u32>().ok());
    match major {
        Some(m) if m >= 12 => "cuda12",
        Some(11) => "cuda11",
        _ => "cpu",
    }
}

/// Moves every file in a tree into one directory, discarding the nesting.
fn flatten(from: &Path, into: &Path) -> Result<()> {
    let mut dirs = vec![from.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if let Some(name) = path.file_name() {
                std::fs::rename(&path, into.join(name))?;
            }
        }
    }
    Ok(())
}

/// Downloads one file, unless it is already there.
///
/// Written to a `.part` beside the destination and renamed once the body ends.
/// An interrupted download that left a full-looking file in place is the exact
/// failure the shell scripts warn about twice, and a rename is how not to have
/// it: a file at the real path has been downloaded completely, always.
async fn get(client: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    if dest.is_file() {
        eprintln!("have  {}", name_of(dest));
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let part = dest.with_extension("part");
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        // An HTML error page written out as if it were a model is a file that
        // exists, passes every check, and fails at load with a parse error.
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;

    let total = resp.content_length();
    let mut file = std::fs::File::create(&part)
        .with_context(|| format!("create {}", part.display()))?;
    let mut stream = resp.bytes_stream();
    let mut done: u64 = 0;
    let mut progress = Progress::start(name_of(dest), total);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("download {url}"))?;
        file.write_all(&chunk)?;
        done += chunk.len() as u64;
        progress.at(done);
    }
    file.sync_all()?;
    drop(file);
    progress.finish(done);
    std::fs::rename(&part, dest)
        .with_context(|| format!("move {} into place", part.display()))?;
    Ok(())
}

/// Downloads an archive and unpacks it into `into`.
///
/// `tar` rather than a zip crate: Windows 10 and later ship bsdtar as `tar.exe`
/// and it reads zip, and `tar` has been on every unix for decades. Three crates
/// and their dependency trees, for a step that runs at most twice.
///
/// `sha256`, when given, must match before anything is extracted.
async fn unpack(client: &reqwest::Client, url: &str, into: &Path, sha256: Option<&str>) -> Result<()> {
    let name = url.rsplit('/').next().unwrap_or("archive");
    // Never reused between runs: a half-downloaded archive unpacks a
    // half-populated directory and still looks like it worked.
    let tmp = std::env::temp_dir().join(format!("ira-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_file(&tmp);
    get(client, url, &tmp).await?;
    if let Some(want) = sha256 {
        use sha2::Digest;
        let got = sha2::Sha256::digest(std::fs::read(&tmp)?);
        let got: String = got.iter().map(|b| format!("{b:02x}")).collect();
        if got != want {
            let _ = std::fs::remove_file(&tmp);
            bail!("{name} is not the file that was pinned (sha256 {got}); refusing to unpack it");
        }
    }
    std::fs::create_dir_all(into)?;

    let status = std::process::Command::new("tar")
        .arg("-xf")
        .arg(&tmp)
        .arg("-C")
        .arg(into)
        .status()
        .context("run tar -- Windows 10 and later ship one, as does every unix")?;
    let _ = std::fs::remove_file(&tmp);
    if !status.success() {
        bail!("tar could not unpack {name}");
    }
    Ok(())
}

fn name_of(path: &Path) -> String {
    path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned()
}

/// The latest progress, for the settings window, which has no terminal to watch.
static LINE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn progress_line() -> String {
    LINE.lock().map(|l| l.clone()).unwrap_or_default()
}

/// One line per file while it downloads.
///
/// Redrawn in place on a terminal, and printed once at each end when the output
/// is a file -- a progress bar rewritten a thousand times into a log is a
/// thousand lines of carriage returns nobody can read.
struct Progress {
    name: String,
    total: Option<u64>,
    tty: bool,
    /// Rate-limits the redraw. A 22 MB download arrives in thousands of chunks
    /// and none of them are worth a syscall of their own.
    last: std::time::Instant,
}

impl Progress {
    fn start(name: String, total: Option<u64>) -> Self {
        let tty = std::io::stderr().is_terminal();
        if !tty {
            eprintln!("fetch {name}{}", match total {
                Some(t) => format!(" ({})", mb(t)),
                None => String::new(),
            });
        }
        Self { name, total, tty, last: std::time::Instant::now() }
    }

    fn at(&mut self, done: u64) {
        if self.last.elapsed().as_millis() < 100 {
            return;
        }
        self.last = std::time::Instant::now();
        if let Ok(mut line) = LINE.lock() {
            *line = match self.total {
                Some(total) => format!("Downloading {}: {} of {}", self.name, mb(done), mb(total)),
                None => format!("Downloading {}: {}", self.name, mb(done)),
            };
        }
        if !self.tty {
            return;
        }
        match self.total {
            Some(total) => eprint!("\rfetch {} {} / {}   ", self.name, mb(done), mb(total)),
            None => eprint!("\rfetch {} {}   ", self.name, mb(done)),
        }
        let _ = std::io::stderr().flush();
    }

    fn finish(&self, done: u64) {
        if self.tty {
            eprintln!("\rfetch {} {}       ", self.name, mb(done));
        }
    }
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every platform IRA claims to run on has a piper to download. A missing
    /// arm here is a first run that gets the models, then stops.
    #[test]
    fn every_supported_platform_has_a_piper() {
        assert!(
            piper_asset().is_some(),
            "no piper asset for {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    }

    /// `have_everything` must be false when a file is missing, or a first run
    /// skips the download and fails the preflight check instead.
    #[test]
    fn a_missing_file_is_noticed() {
        let dir = std::env::temp_dir().join("ira-fetch-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let voice = dir.join("en_US-amy-medium.onnx");
        let piper = dir.join("piper").join("piper");

        assert!(!have_everything(&dir, &voice, &piper), "nothing is there");

        for (_, dest) in core(&dir) {
            std::fs::write(&dest, b"x").unwrap();
        }
        assert!(!have_everything(&dir, &voice, &piper), "piper is still missing");

        std::fs::create_dir_all(piper.parent().unwrap()).unwrap();
        std::fs::write(&piper, b"x").unwrap();
        assert!(have_everything(&dir, &voice, &piper), "everything is there now");

        std::fs::remove_file(&voice).unwrap();
        assert!(!have_everything(&dir, &voice, &piper), "the voice went away");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The archive holds `piper/`, so it has to unpack into the grandparent of
    /// the executable. Off by one directory and it lands in `piper/piper/`.
    #[test]
    fn a_driver_gets_the_pack_that_runs_on_it() {
        let smi = |v: &str| format!("| NVIDIA-SMI 591.86   Driver Version: 591.86   CUDA Version: {v}     |");
        assert_eq!(backend_for(&smi("13.1")), "cuda12");
        assert_eq!(backend_for(&smi("12.4")), "cuda12");
        assert_eq!(backend_for(&smi("11.8")), "cuda11");
        assert_eq!(backend_for(&smi("10.2")), "cpu");
        assert_eq!(backend_for("No devices were found"), "cpu");
    }

    #[test]
    fn piper_unpacks_one_level_above_itself() {
        let piper = Path::new("/data/piper/piper.exe");
        let into = piper.parent().and_then(Path::parent).unwrap();
        assert_eq!(into, Path::new("/data"));
    }
}
