# ira-voice

Duplex voice loop prototype for IRA. Wake word → VAD endpointing → STT → streaming
LLM → streaming TTS, with barge-in.

This exists to answer one question before anything else gets built: **does talking
to it feel right?** If the turn-taking is wrong or the latency is bad, no amount of
tools, memory or UI fixes it. Everything else in IRA is comparatively routine.

## Run

```powershell
.\scripts\fetch-models.ps1
$env:ANTHROPIC_API_KEY = "sk-ant-..."
$env:GROQ_API_KEY = "gsk_..."
cargo run --release
```

Say **"hey Jarvis"**, wait for the chirp, talk. Interrupt her any time.

> **Wear headphones.** There is no acoustic echo cancellation yet, so on speakers
> the mic hears IRA's own voice and she interrupts herself in a loop. This is the
> single biggest gap between this prototype and something you can ship.

## Wiring

```
mic ─┬─ openWakeWord ──── Idle: is that the wake word?
     ├─ Silero VAD ────── Listening: has the user stopped? (700 ms)
     │                    Holding:   has the user started? (250 ms → barge-in)
     └─ utterance buffer ─► Groq Whisper ─► Claude (streaming)
                                                │ sentence at a time
                                                ▼
                                          Piper ─► rodio  (clear() = instant silence)
```

| File | Job | Becomes |
|---|---|---|
| `audio.rs` | cpal capture, downmix, 16 kHz resample | `ira-audio` |
| `wake.rs` | openWakeWord 3-model chain | `ira-wake` |
| `vad.rs` | Silero v5, endpointing + barge-in | `ira-vad` |
| `stt.rs` | Groq Whisper | `ira-stt` (lift Echo's local path in) |
| `llm.rs` | Anthropic stream → sentences | `ira-brain` (Wingman as a library) |
| `tts.rs` | Piper subprocess + rodio | `ira-tts` |
| `main.rs` | state machine | `ira-daemon` |

## Knobs

Everything worth tuning is a `const` at the top of `main.rs`. Tune by ear, not by
theory — these numbers are starting guesses, not measurements.

| Const | Default | Symptom if wrong |
|---|---|---|
| `ENDPOINT_MS` | 700 | Too low: cuts you off mid-thought. Too high: feels sluggish. |
| `BARGE_IN_MS` | 250 | Too low: a cough stops her. Too high: interrupting feels laggy. |
| `BARGE_IN_GRACE_MS` | 300 | Too low: the tail of your question interrupts its own answer. |
| wake threshold | 0.5 | Too low: fires on the TV. Too high: you repeat yourself. |

Env overrides: `IRA_MODELS`, `IRA_WAKEWORD`, `IRA_VOICE`, `IRA_PIPER`.

## Deliberate shortcuts

Each is marked with a `ponytail:` comment at the site.

| Shortcut | Ceiling | Upgrade when |
|---|---|---|
| No AEC, headphones assumed | Unusable on speakers | Before any demo not wearing headphones — `webrtc-audio-processing` |
| Cloud STT only | Not private, dies offline | Lift Echo's local Whisper into `ira-stt` |
| Cheap linear resampler | Slight aliasing | Only if measured word-error-rate suffers |
| Piper respawn on barge-in | ~250 ms before she can speak again | If interruption recovery feels slow — keep a warm spare |
| Fixed 8-turn history window | No real memory | Wire up kortex-memory |
| VAD-only endpointing | Cuts off mid-thought pauses | Add a semantic turn model (smart-turn v2) |

## Wake word

`hey_jarvis` is a pretrained openWakeWord model, used here so the prototype runs
today. A real "IRA" model is a Colab training run against the same three-stage
chain — only `hey_jarvis_v0.1.onnx` changes, no code does.

openWakeWord is Apache-2.0 and free commercially. Porcupine has a built-in
"jarvis" keyword and is far less code, but its commercial licensing does not fit
the plan.

## Tests

`cargo test` — 8 tests, no network or mic needed.

Two run the real ONNX models and are the ones that matter: they check the tensor
shapes threaded through openWakeWord's three stages and Silero's recurrent state
carry. Get one wrong and you find out here instead of at 3am. They skip with a
note if `models/` is empty, so a fresh clone still passes.

The rest cover what silently corrupts audio: resampler ratio, WAV header, and
sentence splitting (which must not break on `3.50`).

The loop itself is tuned by talking to it — there is no test for "feels right".

**Wake latency floor:** openWakeWord needs ~2.2 s of audio in its window before
the classifier can fire at all (76 mel frames to the first embedding, then 16
embeddings at 80 ms). In use the window is always full, so this only shows up in
the first couple of seconds after start-up.
