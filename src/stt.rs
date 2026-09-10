//! Speech-to-text: Groq's hosted Whisper, or any local whisper.cpp server.
//!
//! whisper.cpp's own `whisper-server` speaks the same multipart API as Groq's
//! endpoint -- same `file` part, same `{"text": ...}` back -- so switching
//! engines is a URL, not a code path. Set `IRA_STT_URL` to go local:
//!
//!   whisper-server -m ggml-tiny.en.bin -t 8 --host 127.0.0.1 --port 8231
//!   $env:IRA_STT_URL = "http://127.0.0.1:8231/inference"
//!
//! Local costs latency and buys offline + privacy. Measured on 8 CPU cores,
//! 2.8 s of speech: tiny.en ~750 ms, base.en ~1.5 s. Tune by ear -- this sits
//! directly in the gap between you stopping and IRA starting.

use anyhow::{anyhow, Result};
use reqwest::multipart::{Form, Part};

const MODEL: &str = "whisper-large-v3-turbo";
const GROQ_URL: &str = "https://api.groq.com/openai/v1/audio/transcriptions";

/// Minimal 16-bit PCM WAV. Whisper endpoints want a container, not raw samples.
fn wav(samples: &[f32], sample_rate: u32) -> Vec<u8> {
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
    let form = Form::new()
        .part(
            "file",
            Part::bytes(wav(samples, sr))
                .file_name("turn.wav")
                .mime_str("audio/wav")?,
        )
        .text("model", MODEL)
        .text("response_format", "json");

    // whisper-server ignores the `model` field, so the body is identical for
    // both and only the destination differs.
    let req = match crate::settings::get("IRA_STT_URL").ok_or(()) {
        Ok(url) => client.post(url),
        Err(_) => client.post(GROQ_URL).bearer_auth(
            crate::settings::get("GROQ_API_KEY")
                .ok_or_else(|| anyhow!("GROQ_API_KEY not set"))?,
        ),
    };

    let resp = req
        .multipart(form)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(anyhow!("stt {}: {}", resp.status(), resp.text().await?));
    }
    let v: serde_json::Value = resp.json().await?;
    Ok(v["text"].as_str().unwrap_or_default().trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the local engine accepts *our* WAV bytes, not just a reference
    /// file. Skips unless a server is up:
    ///   whisper-server -m ggml-tiny.en.bin -t 8 --host 127.0.0.1 --port 8231
    ///   IRA_STT_URL=http://127.0.0.1:8231/inference cargo test -- --nocapture
    #[tokio::test]
    async fn local_server_accepts_our_wav() {
        if std::env::var("IRA_STT_URL").is_err() {
            eprintln!("skipped: set IRA_STT_URL to a running whisper-server");
            return;
        }
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
