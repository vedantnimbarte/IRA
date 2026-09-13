//! Speech-to-text: whisper on this machine, or Groq's hosted whisper.
//!
//! [decisions/0020](../docs/decisions/0020-she-runs-whisper-herself.md).
//! `IRA_STT_ENGINE` picks, and local is the default:
//!
//! ```text
//!   local, IRA_STT_URL unset   the whisper-server IRA runs herself (whisper.rs)
//!   local, IRA_STT_URL set     a whisper.cpp-compatible server you run
//!   cloud                      Groq, then local if it fails and local is installed
//! ```
//!
//! **Fallback only moves toward privacy.** Choosing local is choosing that no
//! audio leaves the machine, and an error path that uploads it anyway breaks
//! that at the moment nobody is watching. So local never falls back to Groq;
//! `route` is where that is decided, and its test is what holds it.
//!
//! whisper.cpp's `whisper-server` speaks the same multipart API as Groq's
//! endpoint -- same `file` part, same `{"text": ...}` back -- so the request
//! body is shared and only the destination differs.

use anyhow::{anyhow, Result};
use reqwest::multipart::{Form, Part};

const MODEL: &str = "whisper-large-v3-turbo";
const GROQ_URL: &str = "https://api.groq.com/openai/v1/audio/transcriptions";

/// Whether turns are transcribed on this machine.
pub fn local() -> bool {
    crate::settings::get("IRA_STT_ENGINE").as_deref() != Some("cloud")
}

/// Whether IRA runs the whisper-server herself, rather than using one at a URL.
pub fn managed() -> bool {
    local() && !crate::settings::is_set("IRA_STT_URL")
}

#[derive(Debug, PartialEq)]
enum Via {
    Groq,
    Url(String),
    Managed,
}

/// Where to send a turn, in order. Pure, so the privacy rule is testable.
fn route(local: bool, url: Option<String>, installed: bool) -> Vec<Via> {
    let own = match url {
        Some(u) => Some(Via::Url(u)),
        None => installed.then_some(Via::Managed),
    };
    if local {
        // Not installed still routes there: its error names the fix, where
        // an empty list would say nothing at all.
        vec![own.unwrap_or(Via::Managed)]
    } else {
        std::iter::once(Via::Groq).chain(own).collect()
    }
}

/// Minimal 16-bit PCM WAV. Whisper endpoints want a container, not raw samples.
pub fn wav(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_len = samples.len() as u32 * 2;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend(b"RIFF");
    b.extend(&(36 + data_len).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(&16u32.to_le_bytes()); // fmt chunk size
    b.extend(&1u16.to_le_bytes()); // PCM
    b.extend(&1u16.to_le_bytes()); // mono
    b.extend(&sample_rate.to_le_bytes());
    b.extend(&(sample_rate * 2).to_le_bytes()); // byte rate
    b.extend(&2u16.to_le_bytes()); // block align
    b.extend(&16u16.to_le_bytes()); // bits per sample
    b.extend(b"data");
    b.extend(&data_len.to_le_bytes());
    for &s in samples {
        b.extend(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    b
}

pub async fn transcribe(client: &reqwest::Client, samples: &[f32], sr: u32) -> Result<String> {
    let wav = wav(samples, sr);
    let plan = route(
        local(),
        crate::settings::get("IRA_STT_URL"),
        crate::whisper::installed(),
    );
    let mut last = anyhow!("no speech-to-text configured");
    for via in plan {
        let got = match &via {
            Via::Managed => crate::whisper::transcribe(wav.clone()).await,
            Via::Url(url) => post(client.post(url), &wav).await.map(|t| crate::whisper::clean(&t)),
            Via::Groq => match crate::settings::get("GROQ_API_KEY") {
                Some(key) => post(client.post(GROQ_URL).bearer_auth(key), &wav).await,
                None => Err(anyhow!("GROQ_API_KEY not set")),
            },
        };
        match got {
            Ok(text) => return Ok(text),
            Err(e) => {
                let via = match via { Via::Groq => "groq", Via::Url(_) => "url", Via::Managed => "local" };
                tracing::warn!(via, "stt failed: {e:#}");
                last = e;
            }
        }
    }
    Err(last)
}

async fn post(req: reqwest::RequestBuilder, wav: &[u8]) -> Result<String> {
    let form = Form::new()
        .part(
            "file",
            Part::bytes(wav.to_vec())
                .file_name("turn.wav")
                .mime_str("audio/wav")?,
        )
        // whisper-server ignores `model`, so one body serves both.
        .text("model", MODEL)
        .text("response_format", "json");
    let resp = req.multipart(form).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("stt {}: {}", resp.status(), resp.text().await?));
    }
    let v: serde_json::Value = resp.json().await?;
    Ok(v["text"].as_str().unwrap_or_default().trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The promise local makes: whatever is configured or installed, and however
    /// it fails, no route out of local reaches Groq. Cloud may fall back to
    /// local, and only to local that is actually there.
    #[test]
    fn local_never_falls_back_to_the_cloud() {
        for url in [None, Some("http://127.0.0.1:8231/inference".to_string())] {
            for installed in [false, true] {
                let plan = route(true, url.clone(), installed);
                assert!(!plan.contains(&Via::Groq), "{plan:?}");
                assert_eq!(plan.len(), 1);
            }
        }
        assert_eq!(route(false, None, false), vec![Via::Groq]);
        assert_eq!(route(false, None, true), vec![Via::Groq, Via::Managed]);
        assert_eq!(
            route(false, Some("http://x".into()), false),
            vec![Via::Groq, Via::Url("http://x".into())]
        );
    }

    /// Proves the local engine accepts *our* WAV bytes, not just a reference
    /// file. Skips unless a server is up:
    ///   whisper-server -m ggml-tiny.en.bin -t 8 --host 127.0.0.1 --port 8231
    ///   IRA_STT_URL=http://127.0.0.1:8231/inference cargo test -- --nocapture
    #[tokio::test]
    async fn local_server_accepts_our_wav() {
        // The environment, only here: this is the test's own switch, not a way
        // to configure IRA. It is handed to the settings layer because that is
        // the only thing `transcribe` reads.
        let Ok(url) = std::env::var("IRA_STT_URL") else {
            eprintln!("skipped: set IRA_STT_URL to a running whisper-server");
            return;
        };
        crate::settings::set_in_memory("IRA_STT_URL", &url);
        // A second of silence: whisper returns little, but a malformed
        // container fails the request outright, which is what this checks.
        let out = transcribe(&reqwest::Client::new(), &vec![0.0; 16_000], 16_000).await;
        assert!(out.is_ok(), "{:?}", out.err());
    }

    #[test]
    fn wav_header_is_well_formed() {
        let w = wav(&[0.0, 1.0, -1.0], 16_000);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        assert_eq!(w.len(), 44 + 6);
        // Declared payload size must match the bytes actually appended.
        let declared = u32::from_le_bytes(w[40..44].try_into().unwrap());
        assert_eq!(declared as usize, w.len() - 44);
        // Full-scale input must clamp to i16::MAX, not wrap to a negative.
        assert_eq!(i16::from_le_bytes(w[46..48].try_into().unwrap()), 32767);
    }
}
