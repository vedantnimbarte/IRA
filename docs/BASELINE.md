# Latency baseline

P0's exit criterion. Replaces the guessed targets in [PRD.md](PRD.md) with
measured values, and gives every later phase something to be checked against.

Regenerate with the replay harness described in [TEST-PLAN.md](TEST-PLAN.md);
the numbers below come from the `turn` log line defined in [SPEC.md](SPEC.md).

## Configuration

| | |
|---|---|
| Machine | 8-core CPU, Intel Iris Xe (no CUDA) |
| Date | 2026-09-06 |
| STT | local `whisper-server`, `ggml-tiny.en.bin`, `-t 8` |
| Model | **local stub**, not a real endpoint |
| Utterance | 3.95 s, Piper-synthesised, replayed via `IRA_AUDIO_FILE` |
| Runs | 6, wake word skipped (`IRA_SKIP_WAKE`) |

## Measured

| Stage | Field | Range | p50 |
|---|---|---|---|
| Endpoint → transcript | `stt_ms` | 629 – 700 | ~665 |
| Transcript → first token | `ttft_ms` | 0 | 0 |
| First token → first audio | `tts_ms` | 327 – 356 | ~333 |
| **Endpoint → first audio (NFR-1)** | `total_ms` | **956 – 1056** | **~1002** |

Raw: 956, 976, 993, 1011, 1019, 1056.

## What these numbers do and do not say

**NFR-1's target of < 1200 ms p50 is met — but only because the model is a
stub.** `ttft_ms` is 0 because the stub replies from loopback in under a
millisecond. A real endpoint contributes its own time-to-first-token, and that
lands on top of the 1002 ms measured here. A hosted model answering in 400–800 ms
would put `total_ms` around 1.4–1.8 s, which **misses** the p50 target.

That is the most useful thing this baseline says: the budget is already spent
before the model is even asked. Of the ~1000 ms, two thirds is transcription and
one third is Piper. Streaming STT (P7) attacks the larger half.

**`tts_ms` is not all Piper.** It is everything between the first token and the
first sample: sentence splitting, the write to Piper's stdin, its synthesis, and
the rodio queue. It also absorbs any time the model spends failing, which is why
a failed turn can show a large `tts_ms` alongside `ttft_ms=0`.

## P7 — speculative transcription

Transcription now starts on a pause rather than on proof the turn is over, so it
overlaps the endpoint window. Measured as an interleaved A/B on one machine
rather than against the table above: this box was noticeably slower on the day,
which inflates both arms equally and makes a cross-day comparison meaningless.

Six pairs, alternating, `IRA_SPECULATE_MS=99999` to disable and `200` to enable.

| | median `stt_ms` | median `total_ms` |
|---|---|---|
| Sequential (old behaviour) | 1188 | **1667** |
| Overlapped | 661 | **1145** |

**522 ms saved**, and the overlapped arm was better or tied in all six pairs.
The saving lands on `ENDPOINT_MS - SPECULATE_MS` (500 ms), which is what the
design predicts: the endpoint window is dead time that transcription can occupy.

`stt_ms` remains endpoint-to-transcript — the wait the user experiences, not the
work done. A `spec` field on the turn line says when the transcript was already
in hand, so a small `stt_ms` is not misread as transcription having got faster.

The absolute numbers here are not comparable with the table above, and the
ceiling is: once transcription is shorter than `ENDPOINT_MS` it disappears from
the critical path entirely, and the remaining budget is the model and Piper.

## Not measured

- **Any GPU.** The machine that will run IRA has a GTX 1650. No CUDA pack has
  been executed, so every number here is CPU-only and pessimistic for STT.
- **A real model.** No API key was used. `ttft_ms` has never been observed
  non-zero.
- **Groq STT.** The cloud path is unmeasured; only the local engine has numbers.
- **Wake word latency (NFR-3).** openWakeWord does not fire on synthesised
  speech, so the replay harness cannot measure it. Needs real recordings.
- **Barge-in latency (NFR-2).** Requires a live microphone and speaker.
- **`base.en` and `small.en`.** Only `tiny.en` was run through the harness.
  Earlier ad-hoc `curl` timings suggested `base.en` at roughly 1.5 s, which would
  put `total_ms` near 1.9 s on its own.

## Reproducing

```powershell
whisper-server -m models\ggml-tiny.en.bin -t 8 --host 127.0.0.1 --port 8250
$env:IRA_STT_URL  = "http://127.0.0.1:8250/inference"
$env:IRA_AUDIO_FILE = "corpus\speech-16k.wav"
$env:IRA_SKIP_WAKE = "1"     # openWakeWord ignores synthetic speech
$env:IRA_TAIL_MS   = "5000"  # room for the turn to finish
cargo run --release
```

Each run prints one `turn` line. Take `total_ms` from it, discard the first run
of a configuration (cold page cache), and report p50 and p95 rather than a mean.
