//! Speech-to-text via Groq's hosted Whisper.
//!
//! ponytail: cloud-only for the prototype so the loop's *feel* can be measured
//! without a whisper.cpp build in the way. Echo already has the local path;
//! lifting it into `ira-stt` is the swap, and the router decides which runs.

use anyhow::{anyhow, Result};
use reqwest::multipart::{Form, Part};

const MODEL: &str = "whisper-large-v3-turbo";

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
    let key = std::env::var("GROQ_API_KEY").map_err(|_| anyhow!("GROQ_API_KEY not set"))?;
    let form = Form::new()
        .part(
            "file",
            Part::bytes(wav(samples, sr))
                .file_name("turn.wav")
                .mime_str("audio/wav")?,
        )
        .text("model", MODEL)
        .text("response_format", "json");

    let resp = client
        .post("https://api.groq.com/openai/v1/audio/transcriptions")
        .bearer_auth(key)
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
