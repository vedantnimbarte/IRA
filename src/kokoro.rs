//! Kokoro-82M: the voice, when it is not Piper.
//!
//! [decisions/0021](../docs/decisions/0021-kokoro-is-the-voice.md). Piper reads
//! words; Kokoro, a StyleTTS 2 model trained on far more expressive speech, says
//! them. It runs in this process on the `ort` IRA already loads for the wake word
//! and the VAD, and turns phonemes into 24 kHz audio a sentence at a time.
//!
//! **The phonemes come from Piper's own espeak-ng.** Kokoro takes IPA, not text,
//! and Piper's download already carries espeak-ng and its data for exactly this
//! job, so the library is loaded from there rather than fetched again. That is
//! what kokoro.js and kokoro-onnx do too, including the clean-up afterwards
//! (`kokoro_ipa`).
//!
//! ```text
//!   text ─ split at punctuation ─ espeak-ng → IPA ─ clean-up ─ vocab ids
//!        ─ Kokoro(ids, voice style for that length, speed) ─ 24 kHz samples
//! ```

use anyhow::{anyhow, bail, Context, Result};
use ort::session::Session;
use ort::value::Tensor;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

pub const SAMPLE_RATE: u32 = 24_000;

/// The model's hard limit is 510 tokens with the two pads. A sentence longer than
/// this is split at a space and the halves are spoken back to back.
const MAX_PHONEMES: usize = 500;
/// One style vector per input length, 256 wide.
const STYLE: usize = 256;

/// Pinned: `onnx-community/Kokoro-82M-v1.0-ONNX` at this revision.
pub const REVISION: &str =
    "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/1939ad2a8e416c0acfeecc08a694d14ef25f2231";

/// The voices offered, best first, as (id, what the window calls it). The prefix
/// is the accent and sex: `a`merican or `b`ritish, `f`emale or `m`ale. Each file
/// is 500 KB, so all of them come down with the model.
pub const VOICES: &[(&str, &str)] = &[
    ("af_heart", "Heart · American, female"),
    ("af_bella", "Bella · American, female"),
    ("af_nicole", "Nicole · American, female, soft"),
    ("af_sarah", "Sarah · American, female"),
    ("am_michael", "Michael · American, male"),
    ("am_fenrir", "Fenrir · American, male"),
    ("bf_emma", "Emma · British, female"),
    ("bm_george", "George · British, male"),
];

static MODELS_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn init(models: &Path) {
    let _ = MODELS_DIR.set(models.to_path_buf());
}

pub fn dir() -> PathBuf {
    MODELS_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| crate::paths::in_data("models"))
        .join("kokoro")
}

pub fn model_path() -> PathBuf {
    dir().join("model.onnx")
}

pub fn vocab_path() -> PathBuf {
    dir().join("tokenizer.json")
}

pub fn voice_path(voice: &str) -> PathBuf {
    dir().join(format!("{voice}.bin"))
}

/// The voice the settings name, or Heart.
pub fn voice() -> String {
    crate::settings::get("IRA_KOKORO_VOICE").unwrap_or_else(|| VOICES[0].0.to_string())
}

pub fn installed() -> bool {
    model_path().is_file() && vocab_path().is_file() && VOICES.iter().all(|(v, _)| voice_path(v).is_file())
}

/// Whether Kokoro is the chosen voice. It is the default; Piper is the choice.
pub fn chosen() -> bool {
    crate::settings::get("IRA_TTS_ENGINE").as_deref() != Some("piper")
}

static DOWNLOADING: AtomicBool = AtomicBool::new(false);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// What the settings window says under the voice engine, as (still working,
/// fine, words).
pub fn status() -> (bool, bool, String) {
    if !chosen() {
        return (false, true, "Piper: the quickest voice, and the plainest.".into());
    }
    if DOWNLOADING.load(Ordering::Relaxed) {
        let now = crate::fetch::progress_line();
        return (true, false, if now.is_empty() { "Downloading…".into() } else { now });
    }
    if let Some(e) = LAST_ERROR.lock().ok().and_then(|e| e.clone()) {
        return (false, false, e);
    }
    if !installed() {
        return (false, false, "Kokoro is not downloaded. Piper speaks until it is.".into());
    }
    let name = VOICES.iter().find(|(v, _)| *v == voice()).map_or("", |(_, l)| l);
    (false, true, format!("Kokoro, {name}."))
}

/// Downloads Kokoro when it has just been chosen and is not here. Nothing else
/// needs doing: the speech thread notices the engine and the voice itself.
pub fn apply() {
    if !chosen() || installed() || DOWNLOADING.swap(true, Ordering::Relaxed) {
        return;
    }
    if let Ok(mut e) = LAST_ERROR.lock() {
        *e = None;
    }
    tokio::spawn(async {
        let got = crate::fetch::kokoro_files().await;
        DOWNLOADING.store(false, Ordering::Relaxed);
        if let Err(e) = got {
            tracing::error!("kokoro download failed: {e:#}");
            if let Ok(mut last) = LAST_ERROR.lock() {
                *last = Some(format!("Download failed: {e:#}"));
            }
        }
    });
}

pub struct Kokoro {
    session: Session,
    vocab: HashMap<char, i64>,
    voice: String,
    /// `[510, 256]`, flattened: the style for an input of each length.
    styles: Vec<f32>,
    espeak: &'static Espeak,
}

impl Kokoro {
    /// `piper` is Piper's executable; espeak-ng and its data sit beside it.
    pub fn new(piper: &Path, voice: &str) -> Result<Self> {
        let espeak = Espeak::get(piper.parent().unwrap_or(Path::new(".")))?;
        let vocab: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(vocab_path()).with_context(|| format!("read {}", vocab_path().display()))?,
        )?;
        let vocab = vocab["model"]["vocab"]
            .as_object()
            .ok_or_else(|| anyhow!("no vocab in {}", vocab_path().display()))?
            .iter()
            .filter_map(|(k, v)| Some((k.chars().next()?, v.as_i64()?)))
            .collect();
        // Every core but one: synthesis shares the machine with whisper and the
        // microphone, and a starved capture thread drops audio.
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().saturating_sub(1).max(1));
        // CPU, measured at 0.4x real time on a 6-core Ryzen: each sentence is
        // ready before the one ahead of it finishes playing. DirectML was tried
        // and rejects the model's ConvTranspose upsampling outright.
        let session = Session::builder()?
            .with_intra_threads(threads)
            .map_err(|e| anyhow!("{e}"))?
            .commit_from_file(model_path())
            .with_context(|| format!("load {}", model_path().display()))?;
        let mut k = Self { session, vocab, voice: String::new(), styles: Vec::new(), espeak };
        k.set_voice(voice)?;
        Ok(k)
    }

    /// Switches voice. A file read, so it is cheap enough to check every sentence.
    pub fn set_voice(&mut self, voice: &str) -> Result<()> {
        if voice == self.voice {
            return Ok(());
        }
        let bytes = std::fs::read(voice_path(voice)).with_context(|| format!("no Kokoro voice {voice}"))?;
        if bytes.len() % (4 * STYLE) != 0 {
            bail!("{voice}.bin is not a voice file");
        }
        self.styles = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
        self.voice = voice.to_string();
        Ok(())
    }

    /// One sentence, as 24 kHz mono samples.
    pub fn speak(&mut self, text: &str) -> Result<Vec<f32>> {
        // British voices were trained on British phonemes.
        let lang = if self.voice.starts_with('b') { "en-gb" } else { "en-us" };
        let ipa = kokoro_ipa(&self.espeak.phonemize(text, lang)?, lang == "en-us");
        let mut audio = Vec::new();
        for part in split_long(&ipa, MAX_PHONEMES) {
            let ids: Vec<i64> = part.chars().filter_map(|c| self.vocab.get(&c).copied()).collect();
            if ids.is_empty() {
                continue;
            }
            let n = ids.len();
            let at = n.min(self.styles.len() / STYLE - 1) * STYLE;
            let style = self.styles[at..at + STYLE].to_vec();
            let mut input = Vec::with_capacity(n + 2);
            input.push(0);
            input.extend(ids);
            input.push(0);
            let outs = self.session.run(ort::inputs![
                "input_ids" => Tensor::from_array(([1_usize, n + 2], input))?,
                "style" => Tensor::from_array(([1_usize, STYLE], style))?,
                "speed" => Tensor::from_array(([1_usize], vec![1.0_f32]))?,
            ])?;
            let (_, wave) = outs[0].try_extract_tensor::<f32>()?;
            audio.extend_from_slice(wave);
        }
        Ok(audio)
    }
}

/// Punctuation Kokoro reads as prosody. espeak-ng drops it, so the text is split
/// around it and it is put back between the phonemes, spaces and all.
const PUNCTUATION: &str = ";:,.!?¡¿—…\"«»“”(){}[]";

/// espeak-ng's IPA, adjusted to the phoneme set Kokoro was trained on. The same
/// substitutions kokoro.js makes.
fn kokoro_ipa(ipa: &str, american: bool) -> String {
    let mut s = ipa
        .replace("kəkˈoːɹoʊ", "kˈoʊkəɹoʊ")
        .replace("kəkˈɔːɹəʊ", "kˈəʊkəɹəʊ")
        .replace('ʲ', "j")
        .replace('r', "ɹ")
        .replace('x', "k")
        .replace('ɬ', "l");
    // "two hundred" arrives as one word, and Kokoro slurs it.
    s = insert_before(&s, "hˈʌndɹɪd", |c| c.is_ascii_lowercase() || c == 'ɹ' || c == 'ː');
    // A lone plural "z" belongs on the word before it.
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' '
            && chars.get(i + 1) == Some(&'z')
            && chars.get(i + 2).is_none_or(|c| *c == ' ' || PUNCTUATION.contains(*c))
        {
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    if american {
        // "ninety" is said with a flap in American English.
        out = out.replace("nˈaɪnti", "nˈaɪndi").replace("nˈaɪndiː", "nˈaɪntiː");
    }
    out
}

/// Inserts a space before every `word` whose preceding character matches.
fn insert_before(s: &str, word: &str, prev: impl Fn(char) -> bool) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let mut rest = s;
    while let Some(at) = rest.find(word) {
        let (head, tail) = rest.split_at(at);
        out.push_str(head);
        if out.chars().last().is_some_and(&prev) {
            out.push(' ');
        }
        out.push_str(word);
        rest = &tail[word.len()..];
    }
    out.push_str(rest);
    out
}

/// Splits at spaces into pieces no longer than `max` characters.
fn split_long(s: &str, max: usize) -> Vec<String> {
    let mut parts = vec![String::new()];
    for word in s.split(' ') {
        let last = parts.last_mut().expect("never empty");
        if !last.is_empty() && last.chars().count() + 1 + word.chars().count() > max {
            parts.push(word.to_string());
        } else {
            if !last.is_empty() {
                last.push(' ');
            }
            last.push_str(word);
        }
    }
    parts
}

/// espeak-ng, loaded from Piper's directory.
///
/// One per process and behind a lock: espeak-ng keeps its state in globals and
/// is not safe to call from two threads at once.
type Init = unsafe extern "C" fn(c_int, c_int, *const c_char, c_int) -> c_int;
type SetVoice = unsafe extern "C" fn(*const c_char) -> c_int;
type ToPhonemes = unsafe extern "C" fn(*mut *const c_void, c_int, c_int) -> *const c_char;

pub struct Espeak {
    lock: Mutex<()>,
    set_voice: SetVoice,
    to_phonemes: ToPhonemes,
}

impl Espeak {
    fn get(piper_dir: &Path) -> Result<&'static Espeak> {
        static ESPEAK: OnceLock<Espeak> = OnceLock::new();
        if let Some(e) = ESPEAK.get() {
            return Ok(e);
        }
        let e = Self::load(piper_dir)?;
        Ok(ESPEAK.get_or_init(|| e))
    }

    fn load(piper_dir: &Path) -> Result<Espeak> {
        let lib = ["espeak-ng.dll", "libespeak-ng.so.1", "libespeak-ng.1.dylib", "libespeak-ng.so", "libespeak-ng.dylib"]
            .iter()
            .map(|n| piper_dir.join(n))
            .find(|p| p.is_file())
            .ok_or_else(|| anyhow!("no espeak-ng beside piper in {}", piper_dir.display()))?;
        let handle = dl::open(&lib)?;
        // SAFETY: the signatures are espeak-ng's public API (speak_lib.h), and
        // the library stays loaded for the life of the process.
        unsafe {
            let init =
                std::mem::transmute::<*const (), Init>(dl::symbol(handle, "espeak_Initialize")?);
            let set_voice = std::mem::transmute::<*const (), SetVoice>(dl::symbol(handle, "espeak_SetVoiceByName")?);
            let to_phonemes = std::mem::transmute::<*const (), ToPhonemes>(dl::symbol(handle, "espeak_TextToPhonemes")?);
            // AUDIO_OUTPUT_SYNCHRONOUS: no audio device, no background thread.
            // The path is the directory that *contains* espeak-ng-data.
            let data = CString::new(piper_dir.to_string_lossy().as_bytes())?;
            if init(2, 0, data.as_ptr(), 0) < 0 {
                bail!("espeak-ng would not start with its data in {}", piper_dir.display());
            }
            Ok(Espeak { lock: Mutex::new(()), set_voice, to_phonemes })
        }
    }

    /// IPA for `text`, with its punctuation kept in place.
    pub fn phonemize(&self, text: &str, lang: &str) -> Result<String> {
        let _guard = self.lock.lock().map_err(|_| anyhow!("espeak lock poisoned"))?;
        let voice = CString::new(lang)?;
        // SAFETY: a NUL-terminated voice name; 0 is EE_OK.
        if unsafe { (self.set_voice)(voice.as_ptr()) } != 0 {
            bail!("espeak-ng has no voice {lang}");
        }
        let mut out = String::new();
        let mut words = String::new();
        for c in text.chars() {
            if PUNCTUATION.contains(c) {
                self.flush(&mut words, &mut out)?;
                out.push(c);
            } else {
                words.push(c);
            }
        }
        self.flush(&mut words, &mut out)?;
        Ok(out)
    }

    /// Phonemizes the words gathered so far, keeping the whitespace around them.
    fn flush(&self, words: &mut String, out: &mut String) -> Result<()> {
        let trimmed = words.trim();
        if trimmed.is_empty() {
            out.push_str(words);
            words.clear();
            return Ok(());
        }
        let lead = &words[..words.len() - words.trim_start().len()];
        let tail = &words[words.trim_end().len()..];
        out.push_str(lead);
        let text = CString::new(trimmed)?;
        let mut ptr = text.as_ptr() as *const c_void;
        let mut clauses = Vec::new();
        while !ptr.is_null() {
            // espeakCHARS_UTF8, and phoneme mode bit 1: IPA as UTF-8. Returns one
            // clause per call and advances `ptr`, to null at the end.
            // SAFETY: `ptr` points into `text`, which outlives the loop.
            let p = unsafe { (self.to_phonemes)(&mut ptr, 1, 0x02) };
            if p.is_null() {
                break;
            }
            let clause = unsafe { CStr::from_ptr(p) }.to_string_lossy().trim().to_string();
            if !clause.is_empty() {
                clauses.push(clause);
            }
        }
        out.push_str(&clauses.join(" "));
        out.push_str(tail);
        words.clear();
        Ok(())
    }
}

/// Loading a shared library and finding a symbol in it, per platform.
mod dl {
    use anyhow::{anyhow, Result};
    use std::path::Path;

    #[cfg(windows)]
    pub fn open(path: &Path) -> Result<usize> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH};
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // Altered search path: its own dependencies are found beside it.
        let h = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) };
        if h.is_null() {
            return Err(anyhow!("could not load {}: {}", path.display(), std::io::Error::last_os_error()));
        }
        Ok(h as usize)
    }

    #[cfg(windows)]
    pub fn symbol(handle: usize, name: &str) -> Result<*const ()> {
        use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
        let cname = std::ffi::CString::new(name)?;
        let f = unsafe { GetProcAddress(handle as _, cname.as_ptr() as *const u8) };
        f.map(|f| f as *const ()).ok_or_else(|| anyhow!("{name} missing from espeak-ng"))
    }

    #[cfg(unix)]
    pub fn open(path: &Path) -> Result<usize> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW) };
        if h.is_null() {
            return Err(anyhow!("could not load {}", path.display()));
        }
        Ok(h as usize)
    }

    #[cfg(unix)]
    pub fn symbol(handle: usize, name: &str) -> Result<*const ()> {
        let cname = std::ffi::CString::new(name)?;
        let f = unsafe { libc::dlsym(handle as *mut _, cname.as_ptr()) };
        if f.is_null() {
            return Err(anyhow!("{name} missing from espeak-ng"));
        }
        Ok(f as *const ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn espeak_ipa_becomes_kokoro_ipa() {
        assert_eq!(kokoro_ipa("tˈuː hˈʌndɹɪd", true), "tˈuː hˈʌndɹɪd");
        assert_eq!(kokoro_ipa("tˈuːhˈʌndɹɪd", true), "tˈuː hˈʌndɹɪd");
        assert_eq!(kokoro_ipa("kˈæts z.", true), "kˈætsz.");
        assert_eq!(kokoro_ipa("nˈaɪnti", true), "nˈaɪndi");
        assert_eq!(kokoro_ipa("nˈaɪnti", false), "nˈaɪnti");
        assert_eq!(kokoro_ipa("rˈʌn", true), "ɹˈʌn");
    }

    #[test]
    fn a_long_sentence_is_split_at_spaces() {
        let parts = split_long("aaa bbb ccc", 7);
        assert_eq!(parts, vec!["aaa bbb", "ccc"]);
        assert!(split_long(&"word ".repeat(200), MAX_PHONEMES).iter().all(|p| p.chars().count() <= MAX_PHONEMES));
    }

    /// A real sentence through espeak-ng and the model. Skips unless both are on
    /// disk; writes `kokoro-test.wav` to the temp directory to be listened to.
    #[test]
    fn says_a_sentence() {
        let piper = std::env::var("IRA_PIPER")
            .map(PathBuf::from)
            .unwrap_or_else(|_| crate::paths::in_data(if cfg!(windows) { "piper/piper.exe" } else { "piper/piper" }));
        if !model_path().is_file() || !piper.is_file() {
            eprintln!("skipped: no Kokoro model or no piper");
            return;
        }
        let voice = VOICES.iter().map(|(v, _)| *v).find(|v| voice_path(v).is_file()).unwrap();
        let t = std::time::Instant::now();
        let mut k = Kokoro::new(&piper, voice).unwrap();
        let load = t.elapsed();
        let text = "Sure! It's seventy two degrees and sunny in Pune right now, so it's a good afternoon for a walk.";
        let ipa = k.espeak.phonemize(text, "en-us").unwrap();
        let t = std::time::Instant::now();
        let audio = k.speak(text).unwrap();
        let took = t.elapsed();
        let secs = audio.len() as f32 / SAMPLE_RATE as f32;
        eprintln!("ipa: {ipa}\nload {load:?}, {secs:.2}s of audio in {took:?} (rtf {:.2})", took.as_secs_f32() / secs);
        assert!(secs > 2.0, "far too short for that sentence: {secs}s");

        let out = std::env::temp_dir().join("kokoro-test.wav");
        let mut wav = Vec::new();
        let len = audio.len() as u32 * 2;
        wav.extend(b"RIFF");
        wav.extend((36 + len).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(SAMPLE_RATE.to_le_bytes());
        wav.extend((SAMPLE_RATE * 2).to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(len.to_le_bytes());
        for s in audio {
            wav.extend(((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        std::fs::write(&out, wav).unwrap();
        eprintln!("wrote {}", out.display());
    }
}
