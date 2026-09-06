# IRA Implementation Spec

Types, state transitions, configuration and error handling in enough detail to
implement without re-deciding anything. Where this contradicts
[ARCHITECTURE.md](ARCHITECTURE.md), the architecture document is wrong and should
be corrected.

**Applies to:** P0 – P8
**New modules:** `metrics.rs`, `tool.rs`, `mcp.rs`
**New state:** `Confirming`

## Types

Names match Wingman's deliberately, so lifting its MCP client is mechanical.
`ToolCtx` carries what a tool may read about the current turn; it deliberately
does not carry the TTS handle, so no tool can speak directly.

```rust
// tool.rs -- new at P3

pub struct ToolSpec {
    pub name: String,          // mcp__<server>__<tool> when adapted
    pub description: String,   // truncated to DESC_MAX before use
    pub schema: Value,         // JSON Schema for arguments
    pub mutates: bool,         // resolved from OUR config, never the server
    pub latency: Latency,
}

pub enum Latency {
    Fast,        // < 300 ms   -- no filler
    Slow,        // < 15 s     -- filler phrase first
    Background,  // unbounded  -- returns Started(JobId)
}

pub enum ToolOutcome {
    Speak(String),   // read this out
    Silent,          // done; acknowledge with a tone, say nothing
    Started(JobId),  // result arrives via the job channel
}

pub struct ToolCtx {
    pub turn: TurnId,
    pub transcript: String,          // what the user actually said
    pub cancel: CancellationToken,   // cancelled on barge-in
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome>;
}

pub const DESC_MAX: usize = 1024;
pub const FAST_BUDGET_MS: u64 = 300;
pub const SLOW_BUDGET_MS: u64 = 15_000;
```

> **Cancellation is not the same as undo.** `ctx.cancel` fires on barge-in, but a
> tool that has already sent an email cannot unsend it. Tools must treat
> cancellation as "stop before the next side-effect", and any tool whose
> side-effect is not cancellable must be `mutates: true` so it passes the
> confirmation gate before starting.

## State transitions

Exhaustive. Any state/event pair not listed is a no-op. Events are evaluated per
audio frame except where marked otherwise.

| State | Event | → State | Actions |
|---|---|---|---|
| Idle | `wake_score > 0.5` | Listening | Chirp; reset VAD; seed utterance with 400 ms pre-roll; clear counters |
| Listening | `vad_speech` | Listening | `heard_speech = true`; `silence_ms = 0` |
| Listening | `vad_silence && heard_speech` | Listening | `silence_ms += 32` |
| Listening | `silence_ms >= 700` | Holding | Fresh cancel token; spawn turn; reset VAD; start turn timer |
| Listening | `utterance_ms >= 20_000` | Holding | As above; log truncation |
| Listening | `!heard_speech && 3_000 ms` | Idle | Discard utterance; **no sound** (a false wake must not announce itself) |
| Holding | `barge_ms >= 250 && past grace` | Listening | Cancel token; interrupt TTS; reset VAD; seed utterance from full 1 s pre-roll; `heard_speech = true` |
| Holding | reply done && tts idle | Listening † | Open follow-up window (P2). Falls through to Idle when it expires |
| Holding | tool wants mutate | Confirming | Speak the confirmation question; start 6 s timer; hold the pending call |
| Confirming | transcript ∈ YES | Holding | Execute the held call; resume the turn |
| Confirming | transcript ∈ NO | Idle | Drop the call; say "Cancelled."; end the turn |
| Confirming | unrecognised reply | Confirming | Re-ask once, then treat as NO. **Ambiguity fails closed.** |
| Confirming | 6 s timeout | Idle | Drop the call; say nothing further |
| Confirming | `barge_in` | Listening | Drop the call. **Interrupting a confirmation is never consent** |
| any | `job_complete` (P8) | unchanged | Completion tone now; speak the summary on next entry to Idle |

† Before P2 this transition targets Idle. The follow-up window is the only change
P2 makes to this table.

### Confirmation grammar

Matched case-insensitively against the whole trimmed transcript, not a substring
search — "no, don't do that" must not match on "do that".

| Class | Accepted |
|---|---|
| YES | `yes` `yeah` `yep` `yup` `sure` `ok` `okay` `do it` `go ahead` `confirm` `send it` |
| NO | `no` `nope` `nah` `cancel` `stop` `don't` `do not` `never mind` `forget it` |
| Anything else | Re-ask once, then NO |

## Errors and what the user hears

FR-9 is that no failure is silent. This table is the whole implementation of that
requirement. Phrases are fixed strings, not model output — an LLM failure must not
need the LLM to report itself.

| Condition | Response | User hears |
|---|---|---|
| STT request failed | Speak | "I didn't catch that." |
| STT returned empty text | Tone | Low double tone. Probably noise after a false wake; a sentence here would be worse than a sound |
| Model request failed | Speak | "I'm having trouble thinking right now." |
| Model stream broke mid-reply | Speak | Nothing new — finish the sentences already queued, then stop. A half-reply plus an apology is worse than a half-reply |
| Tool exceeded its latency budget | Speak | "That's taking too long, so I stopped." |
| Tool returned an error | Speak | "That didn't work." Detail goes on screen and to the log, not into speech |
| MCP server unreachable at turn time | Speak | "I can't reach that right now." |
| Piper died / stdin closed | Tone | Error earcon, then respawn once. **TTS cannot announce its own failure** — this is why an earcon path must exist independently of speech |
| Audio output device lost | Fatal | Nothing is audible by definition. Log, surface on screen, exit non-zero |
| Missing key or model at startup | Fatal | Refuse to start with a message naming the fix. Never fail at turn time for something knowable at boot |

### Three earcons

| Sound | Shape | Means |
|---|---|---|
| Wake chirp | rising, 660 → 880 Hz | Wake word landed, floor is yours |
| Error earcon | falling, 440 → 330 Hz | Something failed and speech could not say so |
| Job pip | single soft 880 Hz | A background job finished |

All three distinguishable with your back to the machine.

## Configuration

### Environment variables

Complete current surface. Everything path-shaped resolves relative to the working
directory.

| Variable | Default | Effect |
|---|---|---|
| `IRA_MODELS` | `models` | Directory holding all ONNX weights and the voice |
| `IRA_WAKEWORD` | `hey_jarvis_v0.1.onnx` | Classifier filename inside `IRA_MODELS` |
| `IRA_VOICE` | `en_US-amy-medium.onnx` | Piper voice; sample rate read from the sidecar JSON |
| `IRA_PIPER` | `piper/piper.exe` | Piper executable |
| `IRA_STT_URL` | *unset* | Set → local whisper.cpp; unset → Groq |
| `GROQ_API_KEY` | *required\** | \*Unless `IRA_STT_URL` is set |
| `IRA_LLM_URL` | *unset* | Set → OpenAI wire format; unset → Anthropic |
| `IRA_LLM_KEY` | *unset* | Bearer token for `IRA_LLM_URL`; omit for a local server |
| `IRA_LLM_MODEL` | `claude-sonnet-5` | Required with `IRA_LLM_URL` — gateways name models differently |
| `ANTHROPIC_API_KEY` | *required\** | \*Unless `IRA_LLM_URL` is set |
| `IRA_CONFIG` | `ira.toml` | *P4* — tool and server configuration |
| `IRA_AUDIO_FILE` | *unset* | *P0* — replay a WAV instead of opening the mic (see [TEST-PLAN.md](TEST-PLAN.md)) |
| `RUST_LOG` | `ira=info` | Must match the crate name; a rename silently disables logging |

### `ira.toml` — new at P4

```toml
# Timing. Every value here is currently a const in main.rs.
[timing]
endpoint_ms        = 700
barge_in_ms        = 250
barge_in_grace_ms  = 300   # measured from first audio out, not turn start
follow_up_ms       = 2000  # P2
confirm_timeout_ms = 6000

[wake]
threshold = 0.5

# Tools IRA implements itself.
[tools.memory]
enabled = true
mutates = false
latency = "fast"

# MCP servers. stdio spawns a child; http uses Streamable-HTTP.
[[mcp.server]]
name      = "calendar"
transport = "stdio"
command   = "mcp-calendar"
args      = []

# mutates is declared HERE, per tool, and never read from the server.
# Anything not listed defaults to mutates = true and will ask first.
[mcp.server.tools]
list_events  = { mutates = false, latency = "slow" }
create_event = { mutates = true,  latency = "slow" }

[[mcp.server]]
name      = "wingman"
transport = "http"
url       = "http://127.0.0.1:8787"

[mcp.server.tools]
exec = { mutates = true, latency = "background" }
```

## Instrumentation — P0

One structured event per turn, at `info`. All durations are milliseconds from the
previous marked point, so the fields sum to `total_ms` and a regression shows
which stage moved.

```rust
// One line per turn. Field names are the contract; the
// benchmark harness in the test plan parses these.
tracing::info!(
    turn        = %turn_id,
    wake_ms,          // chirp emitted after wake fired
    listen_ms,        // wake -> endpoint decision
    stt_ms,           // endpoint -> transcript in hand
    ttft_ms,          // transcript -> first model token
    tts_ms,           // first token -> first audio sample out
    total_ms,         // endpoint -> first audio  == NFR-1
    tool_ms,          // 0 when no tool ran
    stt_backend = %backend,   // "groq" | "local"
    llm_model   = %model,
    tools       = tool_count,
    barged      = barged_in,
    "turn"
);
```

`total_ms` is NFR-1: end of user speech to first audible syllable. Every other
field exists to explain a bad `total_ms`. Report p50 and p95 over a run, never a
mean — the tail is what users remember, and one 4-second turn is more damaging
than twenty 900 ms ones are good.

## Sentence splitting

Already implemented and tested; specified here because tool-calling in P3 must not
break it. The unit of speech is a sentence, which is what allows playback to start
before generation finishes.

- Flush at `.` `!` `?` **only** when followed by whitespace or end of buffer —
  this is what keeps `3.50` and `e.g.` from becoming separate utterances.
- Require at least one character before the mark, so a leading period never
  flushes empty.
- Above 160 buffered characters with no sentence end, flush at the last space and
  keep the trailing partial word.
- On cancellation, the trailing buffer is discarded, never spoken.

> **P3 constraint that follows from this.** A spoken sentence cannot be recalled.
> The model must therefore either speak or call a tool — never narrate half a
> sentence and then decide to call one. Enforce it in the system prompt and drop
> any text delta arriving in the same content block as a tool call.

## Acceptance criteria

Each phase is done when every line below is demonstrably true. No partial credit.

### P0 — Instrumentation
- Every completed turn emits one `turn` event with all fields populated.
- `ira doctor` exits non-zero and names the fix for: missing model file, no input
  device, missing required key, unreachable `IRA_STT_URL`.
- A baseline table of p50/p95 `total_ms` for both STT backends is committed.

### P1 — Honesty
- Every row of the error table produces its specified sound, verified by fault
  injection.
- A sentence generated before a barge-in is never spoken after it — proved by a
  test that queues a sentence, cancels, starts a new turn, and asserts silence.
- `BARGE_IN_GRACE_MS` is measured from first audio out; a turn whose STT takes 2 s
  still has its full grace window.
- The system prompt contains no claim IRA cannot fulfil.

### P2 — Follow-up turns
- A three-turn exchange completes with one wake word.
- Silence through the follow-up window returns to Idle without speaking.
- Room noise during the window does not open a turn.

### P3 — Tool foundation
- A `Fast` read tool answers with no filler and no confirmation.
- A `Slow` tool speaks a holding phrase, then the result.
- A `mutates` tool asks, executes on yes, and aborts on no, on timeout, on an
  unrecognised answer twice, and on barge-in.
- A tool exceeding its budget is abandoned and reported aloud.
- Memory recall returns a fact from a prior session.

### P4 — MCP
- A third-party server's tool is callable after editing only `ira.toml`.
- A tool absent from `[mcp.server.tools]` is treated as `mutates = true`.
- A description longer than `DESC_MAX` is truncated before reaching the model.
- An unreachable server at startup logs and degrades; it does not prevent boot.

### P5 — Companion UI
- The live transcript updates within one sentence of speech.
- A long answer is spoken in at most two sentences and rendered in full.
- The window takes no focus and does not interrupt the loop when closed.

### P6 — AEC
- A full three-turn conversation on speakers at normal volume with zero
  self-interruptions.
- Barge-in still meets NFR-2 with AEC in the path — measured, not assumed.
- Press-to-talk works as an alternative activation mode.

### P7 — Turn quality
- p50 `total_ms` improves against the P0 baseline on the same corpus.
- A deliberate 900 ms mid-sentence pause does not end the turn.
- The wake model responds to "IRA" with false wakes still under NFR-4.

### P8 — Background jobs
- A background tool returns `Started` and the loop accepts a new turn immediately.
- Completion emits the tone at once and the spoken summary at the next Idle.
- A job outliving the process is reported as lost rather than silently dropped.

## Module change list

| Phase | Files touched | Nature |
|---|---|---|
| P0 | `metrics.rs` (new), `main.rs`, `doctor.rs` (new), `audio.rs` | Additive; no behaviour change |
| P1 | `main.rs`, `llm.rs`, `tts.rs` | Bug fixes plus the error/earcon table |
| P2 | `main.rs` | One transition and one timer |
| P3 | `tool.rs` (new), `llm.rs`, `main.rs` | Largest change: generation becomes a loop; new state |
| P4 | `mcp.rs` (new), `config.rs` (new) | Additive behind the trait |
| P5 | new binary or crate, `main.rs` (IPC) | Separate surface; loop unaffected when absent |
| P6 | `audio.rs`, `tts.rs` | Only phase touching the closed audio path |
| P7 | `stt.rs`, `vad.rs`, `models/` | Streaming; second turn model; retrained classifier |
| P8 | `tool.rs`, `main.rs` | Job table; proactive speech |

P6 is the only phase touching `audio.rs`, and it is independent of P3–P5. Two
developers can work in parallel across that seam without conflicts.
