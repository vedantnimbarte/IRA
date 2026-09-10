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
    Answer(String),  // a result; handed back to the model, not read verbatim
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

Every transition the loop makes. Any state/event pair not listed is a no-op.
Events are evaluated per audio frame except where marked otherwise.

| State | Event | → State | Actions |
|---|---|---|---|
| Idle | a job report is queued | Holding | Speak it; no turn is started, so no `turn` line is emitted |
| Idle | `wake_score > 0.5`, or the talk control | Listening | Chirp; reset VAD; seed utterance with 400 ms pre-roll; clear counters |
| Listening | `vad_speech` | Listening | `heard_speech = true`; `silence_ms = 0`; bump the utterance generation |
| Listening | `vad_silence && heard_speech` | Listening | `silence_ms += 32` |
| Listening | `silence_ms >= 200`, nothing in flight for this generation | Listening | **Start transcribing.** The turn may not be over; if it is not, the result is dropped as stale |
| Listening | `silence_ms >= 700` | Holding | Start the turn and wait for a transcript — which may already be in hand |
| Listening | `utterance_ms >= 20_000` | Holding | As above; log truncation |
| Listening | `!heard_speech && 3_000 ms` after a wake word | Idle | Discard utterance; **no sound** (a false wake must not announce itself) |
| Listening | `!heard_speech && 2_000 ms` in a follow-up | Idle | Discard utterance; **no sound**. Shorter than the post-wake wait: a wake word is a promise to speak, a finished reply is not |
| Holding | transcript arrives for this generation | Holding | Record `stt_ms`; begin the reply. An empty one gets a tone, a failed one an apology |
| Holding | no transcript after 15 s | Holding | Say "I didn't catch that". The transcriber always answers, so this means it died |
| Holding | `barge_ms > 0` (not under `IRA_PTT`) | Holding | Duck to 35 %. Restore if the speech stops without becoming an interruption |
| Holding | `barge_ms >= 250 && past grace`, or the talk control | Listening | Cancel token; log the turn as barged; interrupt TTS; abandon any pending transcript; reset VAD; seed utterance from the full 1 s pre-roll |
| Holding | a tool wants to change something | Confirming | Speak the question; hold the pending call; reset the utterance buffer |
| Holding | reply done && tts idle && no transcript pending | Listening | Log the turn; open the follow-up window; seed from pre-roll |
| Confirming | `vad_speech` / `vad_silence` | Confirming | Same buffering as Listening, with a 6 s no-speech deadline |
| Confirming | endpoint | Confirming | Transcribe the answer only — no model, no history, no tools |
| Confirming | transcript ∈ YES | Holding | Run the held call; the turn resumes where it paused |
| Confirming | transcript ∈ NO | Holding † | Say "Cancelled."; refuse the call; cancel the turn |
| Confirming | unrecognised reply | Confirming | Re-ask once, then refuse. **Ambiguity fails closed.** |
| Confirming | 6 s no speech | Idle | Refuse the call; say nothing. Silence is not consent |
| any | a background job finishes | unchanged | Pip immediately; queue the report for the next visit to Idle |

† Not Idle, as originally specified: the turn returns to Holding so "Cancelled."
is actually heard, and the follow-up window then opens as it does after any
reply — which is what lets the user immediately say what they *did* want.

A confirmation is transcribed and matched against the grammar directly. It never
reaches the model, so the thing being confirmed gets no chance to argue its way
past the question.

The no-speech deadline stops applying the instant speech is heard, or a slow
speaker gets cut off mid-sentence. `listening_next` in `main.rs` is that
decision, extracted so the interaction between the three exits can be tested
without a microphone; `should_interrupt` is the same for barge-in.

The follow-up window also covers the failure phrases: after "I didn't catch
that", the floor is already open and the user can simply say it again.

### Confirmation grammar

Matched case-insensitively against the whole trimmed transcript, not a substring
search — "no, don't do that" must not match on "do that".

| Class | Accepted |
|---|---|
| YES | `yes` `yeah` `yep` `yup` `sure` `ok` `okay` `do it` `go ahead` `confirm` `send it` `please do` |
| NO | `no` `nope` `nah` `cancel` `stop` `don't` `do not` `never mind` `forget it` |
| Anything else | Re-ask once, then NO |

Punctuation and case are stripped before matching, and **an answer made only of
one word repeated is that word**. Whisper repeats a short utterance over the
silence that follows it, so a perfectly clear "No." arrives as
"No. No. No. No. No."; an exact match reads that as gibberish and fails closed
for the wrong reason. With a real microphone the trailing silence is
guaranteed, which makes the repetition the normal case rather than the odd one.

Repetition of something that is not an answer is still not an answer, and a mix
of yes and no words is not an answer either.

The deadline starts when IRA **finishes asking**, not when it starts. It is how
long the user gets to answer, so it begins when they can — counting from the
start of the question spends part of their time on IRA's own voice, which is
the same error as measuring the barge-in grace from the start of a turn.

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
| Job pip | single soft 880 Hz | A background job finished; the words come at the next `Idle` |

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
| `IRA_PIPER` | `piper/piper.exe`, or `piper/piper` off Windows | Piper executable |
| `IRA_STT_URL` | *unset* | Set → local whisper.cpp; unset → Groq |
| `GROQ_API_KEY` | *required\** | \*Unless `IRA_STT_URL` is set |
| `IRA_LLM_URL` | *unset* | Set → OpenAI wire format; unset → Anthropic |
| `IRA_LLM_KEY` | *unset* | Bearer token for `IRA_LLM_URL`; omit for a local server |
| `IRA_LLM_MODEL` | `claude-sonnet-5` | Required with `IRA_LLM_URL` — gateways name models differently |
| `ANTHROPIC_API_KEY` | *required\** | \*Unless `IRA_LLM_URL` is set |
| `IRA_AUDIO_FILE` | *unset* | Replay a WAV instead of opening the mic (see [TEST-PLAN.md](TEST-PLAN.md)) |
| `IRA_CLOCK` | *unset* | `virtual` drops replay pacing, for CI |
| `IRA_CONFIG` | `ira.toml` | MCP servers and per-tool policy |
| `IRA_UI` | `8180` | Screen port. `off` disables it entirely |
| `IRA_ORB` | *unset* | `off` disables the overlay. Windows only ([0013](decisions/0013-the-orb-is-an-overlay-on-the-same-stream.md)) |
| `IRA_SETTINGS` | `ira.local.toml` | Where the settings window saves URLs and model ids. Keys are never in it ([0014](decisions/0014-settings-are-editable-while-she-runs.md)) |
| `IRA_TRANSCRIPT` | `transcript.jsonl` | Where the conversation is recorded. `off` disables it |
| `IRA_PTT` | *unset* | Set to disarm voice barge-in. Interrupting becomes the talk control, which is what makes speakers usable without echo cancellation |
| `IRA_SPECULATE_MS` | `200` | Silence after which transcription starts. Above `ENDPOINT_MS` disables speculation, which is how the two are compared on one machine |
| `IRA_TAIL_MS` | `3000` | Silence appended after a replayed file. The model's round trip happens inside this window, so a benchmark wanting the whole reply needs more |
| `IRA_SKIP_WAKE` | *unset* | Start in Listening. openWakeWord does not fire on synthesised speech, so a Piper corpus never gets past Idle |
| `IRA_WINGMAN_URL` | *unset* | `wingman serve`, e.g. `http://127.0.0.1:8787`. Unset → no `wingman` tool at all |
| `IRA_WINGMAN_TOKEN` | *unset* | Bearer token. Required only if `/v1/health` reports `auth_required` |
| `IRA_WINGMAN_PROJECT` | *first listed* | Which project a coding task goes to. Wingman's own allowlist decides what is reachable |
| `RUST_LOG` | `ira=info` | Must match the crate name; a rename silently disables logging |

### `ira.toml`

Optional. Without it IRA runs with its built-ins and nothing else. Unknown keys
are rejected rather than ignored: a typo in a `mutates` line would otherwise
disarm the confirmation gate silently.

Windows paths need TOML *literal* strings (single quotes) — a backslash in a
basic string is an escape.

```toml
# MCP servers. stdio spawns a child; http uses Streamable-HTTP.
[[mcp.server]]
name      = "calendar"
transport = "stdio"
command   = "mcp-calendar"
args      = []

# Which tools to expose. Omit or leave empty for all of them.
#
# Every schema is sent to the model on every round, and a turn that calls a
# tool has two rounds. A server offering sixteen tools puts sixteen schemas in
# front of the model twice per turn, which is worth choosing deliberately.
only = ["list_events", "create_event"]

# What IRA believes about each tool, regardless of what the server says.
# Anything not listed defaults to mutates = true and latency = "slow", so it
# asks before running.
[mcp.server.tools]
list_events  = { mutates = false, latency = "fast" }
create_event = { mutates = true,  latency = "slow", confirm = "Add that to your calendar?" }

[[mcp.server]]
name      = "kortex"
transport = "http"
url       = "http://127.0.0.1:8765"

[mcp.server.headers]
Authorization = "Bearer ..."
```

### kortex-memory

The memory store named throughout the roadmap. It offers sixteen tools; a voice
loop should carry two. Every schema is sent to the model on every round and a
tool-calling turn has two rounds, so exposing all sixteen would put thirty-two
schemas in front of the model per turn.

```toml
[[mcp.server]]
name      = "kortex"
transport = "stdio"          # or "http" against http://localhost:8765/sse
command   = "kortex-mcp"
only      = ["recall", "remember"]

[mcp.server.tools]
recall   = { mutates = false, latency = "slow" }
remember = { mutates = true,  latency = "slow", confirm = "Shall I remember that?" }
```

The other fourteen — `get_memory` by UUID, `list_memories`, `update_memory`,
`delete_memory`, `link_memories`, `pin_memory`, the session and attachment
tools, `get_context_bundle` — are things nobody does by voice. They stay
available to whatever else talks to kortex.

This has been verified against kortex's tool surface but **not against kortex
itself**; see [ROADMAP.md](ROADMAP.md#open-questions).

### Wingman

Wingman is an MCP *client*, not a server, so it is the one capability that
cannot arrive as a line of `ira.toml`. It gets `src/wingman.rs`, which speaks
its HTTP API and implements the same `Tool` trait as everything else, so the
registry and the model cannot tell it apart from an MCP tool.

| Route | Used for |
|---|---|
| `GET /v1/health` | Is a daemon there, and will it want a token. The one unauthenticated route |
| `GET /v1/projects` | The allowlist, when `IRA_WINGMAN_PROJECT` is unset |
| `POST /v1/projects/{id}/turns` | `{prompt, model, mode}` in, typed SSE events out |

One tool, `wingman`, taking one string. It is declared `mutates = true` with
`latency = "background"`:

- **Mutating** because it edits files and runs commands. That is the most
  expensive thing a misheard sentence could cause, so it goes through the same
  spoken confirmation as any other write.
- **Background** because a coding turn takes minutes. It returns
  `Started`, the loop takes further turns while it runs, and the result is
  spoken at the next Idle like any other job (see [Background jobs](#background-jobs)).

A refused turn — a busy session, a spend ceiling — comes back as a JSON error
rather than an empty stream, and is reported as a refusal rather than as
silence.

The stream is `wingman_core::AgentEvent` in snake case, one JSON object per
`data:` line, the `event:` name being the payload's own `type`. IRA reads five
of the nine:

| Event | What IRA does |
|---|---|
| `text_delta` | Collected. This is the answer |
| `verification` | Appended as "Checks passed/failed" plus the summary. It is the difference between "it wrote something" and "it works" |
| `stop` | `end_turn` is the only clean finish. `max_turns`, `max_tokens` and `gate_failed` are reported as stopping short |
| `error` | The turn failed, whatever the HTTP status said |
| `end` | A non-zero `exit` is a failure; the last line of `stderr` says why |

`thinking_delta` is the model's working-out rather than its answer, and
`tool_start`, `tool_result`, `usage` and `turn_complete` are machinery. None of
them are spoken.

**A failing turn returns 200.** An unreachable provider, a rejected key and a
red gate all arrive as an `error` event inside a successful stream, so judging
by the status code alone reports a dead turn as a success with nothing to say.

Verified against `wingman serve` 0.3.0: connection, project discovery, the
request shape, the error path, and a turn that runs to `stop: end_turn` and is
spoken at the next Idle.

No provider key is needed to reproduce that last one. Wingman's local backends
take a base URL instead of a key, so any OpenAI-compatible endpoint will do:

```
wingman login llamacpp --base-url http://127.0.0.1:8299 --model stub
```

What that leaves untested is a turn driven by a **real** model. With a stub
behind it Wingman answers and stops, so it never calls a tool, never edits a
file and never runs its verification gate — which means the `verification`
event, the one worth speaking, has still only been seen from a stub. See
[ROADMAP.md](ROADMAP.md#open-questions).

Timing and the wake threshold stay `const`s in `main.rs`. They are tuned by ear
against the `turn` line, not by anyone editing a file, and config surface nobody
has asked for is surface to keep working.


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

## The screen

IRA serves one page and one event stream on `127.0.0.1:8180`, loopback only —
it carries a live transcript of everything said in the room.

- `GET /` — the page.
- `GET /events` — server-sent events, one JSON object per message, discriminated
  by `kind`: `state`, `heard`, `reply`, `tool`, `result`, `confirm`,
  `answered`, `failed`, `turn`.
- `POST /talk` — takes the floor, or interrupts if IRA is speaking. Answers
  `204`. The page has a button; anything else can use it too, which is how a
  global hotkey is bound without IRA taking a platform input dependency:

  ```
  curl -X POST http://127.0.0.1:8180/talk
  ```

  The press is consumed by the next audio frame rather than acting immediately,
  so it reuses the state machine rather than duplicating it. At 80 ms frames
  that is well inside the interruption budget.

A page connecting mid-conversation is replayed the last 200 events, so opening
it shows what just happened rather than an empty screen.

**Nothing here may affect the loop.** A closed tab, a stalled reader or a
browser that never connects are all a dropped socket. A page that falls behind
misses events; the broadcast channel drops for a slow reader rather than
applying backpressure.

**The screen clause in the system prompt is conditional on a page being
connected.** `Ui::watchers()` is read as the turn starts, and only a non-zero
count selects the wording that tells the model to say the short version aloud
and leave the rest on screen. Promising a screen nobody is watching is the same
defect P1 removed, with extra steps.

## The transcript

One JSON object per line, appended per turn: `at`, `turn`, `user`, `ira`,
`tools`, `in_tokens`, `out_tokens`. JSONL because it appends without rewriting,
survives truncation mid-write, and `tail -f` reads it while IRA is still
talking.

It carries **no timing**. A reply is recorded when the model finishes, which is
before a word of it has been spoken, so anything measured there would be read
too early and would mostly be zero. Latency lives in the `turn` log line, which
is emitted when the turn is actually over.

This is text, not audio — audio is still buffered for one utterance and
discarded. But a conversation on disk is a change in posture from a process that
kept nothing, so the path is logged at start-up rather than left to be
discovered.

Token counts appear when the endpoint volunteers them. Anthropic does so
unprompted, on `message_start` and `message_delta`. OpenAI-compatible endpoints
report usage only when asked with `stream_options`, which IRA does not send: an
unknown field would break a gateway that rejects them, and a working
conversation is worth more than a token count. Those setups report zero.

There is no cost figure, only tokens. Pricing means a rate table that goes stale
silently and errs toward under-reporting.

## Background jobs

A tool declaring `latency = "background"` is detached by the host, which returns
`Started(id)` at once so the loop can take another turn. The job runs with its
own cancellation token, not the turn's: it was agreed to before it started, and
interrupting the sentence that asked for it is not a reason to abandon it.

When it finishes:

1. A pip sounds immediately, wherever the conversation is.
2. The report joins a queue.
3. At the next entry to `Idle` — not mid-reply, not while the user is speaking —
   one report is spoken, and IRA enters `Holding` so it can be interrupted and
   so the follow-up window opens afterwards.

Jobs do not survive the process. On shutdown anything still running is counted
and reported, because an unreported job is indistinguishable from a failed one.

Nothing in this is specific to any tool: `latency = "background"` in `ira.toml`
is the whole interface.

## Transcription runs ahead of the endpoint

Transcription starts when speech *pauses* for `SPECULATE_MS`, not when the turn
is proven over at `ENDPOINT_MS`. The two therefore overlap, and the saving is
the gap between them.

Every scrap of speech bumps a generation counter, so a transcript made before
the user carried on talking is recognised as describing a sentence that no
longer exists, and dropped. A pause mid-sentence produces a wasted
transcription; that costs CPU which was otherwise idle and no wall-clock at all.

At the endpoint there are three cases, and they converge on one path:

| At the endpoint | What happens |
|---|---|
| A transcript is ready for this generation | Reply begins immediately; `stt_ms` is near zero and `spec=true` |
| One is in flight for this generation | The turn waits for it; `stt_ms` is what is left of it |
| Neither | One is started and waited for — the old behaviour |

`stt_ms` stays what it always was: endpoint to transcript in hand, which is the
wait the user actually experiences. `spec` says why it is small, so nobody reads
the log and concludes transcription got faster.

A turn gives up on a transcript after `TRANSCRIPT_TIMEOUT_MS`. The transcribing
task always answers, even to report failure, so reaching that means the task
died — and without the timeout the loop would hold the floor forever, which is
the worst thing IRA can do.

## Speaking and stopping

`Tts::idle()` decides when a reply is over, and it has two cases rather than
one. With nothing outstanding, an empty queue plus 400 ms of quiet means
finished. With a sentence handed to piper that it has not started rendering,
an empty queue means *not yet* — piper synthesises at roughly a tenth of real
time, so a long sentence takes over a second to begin. Judging that by the
400 ms rule abandons the reply before it says a word, and the longer the answer
the more certain the failure. That case waits up to 10 s, bounded so a dead
piper cannot strand the loop holding the floor.

Interruption has two stages. At the first hint of speech TTS ducks to 35 %; only
a confirmed `BARGE_IN_MS` of speech cuts it off. A cough gets the volume back
and costs nothing. Ducking is disabled under `IRA_PTT`, where the speech the
microphone hears is most likely IRA's own.

## Tool loop

The model may ask for a tool instead of answering. `llm.rs` runs up to
`MAX_ROUNDS` (3) request rounds per turn, appending the call and its result to
the conversation each time, then answers with whatever it has.

- Tools are advertised only when the registry is non-empty.
- A `Slow` tool speaks `FILLER` ("Let me check.") *before* the wait, not after.
  Fixed text, because a filler needing a model round trip defeats its purpose.
- Every call is wrapped in its latency class's budget and abandoned on overrun.
- A tool error is handed back to the model to phrase, so a failure stays inside
  the conversation instead of ending it on a canned line.
- `mutates` tools go through `Confirming` first. A refusal cancels the turn.

Both wire formats stream tool calls as fragments — Anthropic as
`input_json_delta` inside a `tool_use` block, OpenAI as `tool_calls` deltas — so
arguments are accumulated across frames and parsed once at the end.

## VAD input contract

Silero v5's ONNX takes **576 samples per step, not 512**: the 64 samples
preceding the chunk are prepended as context, exactly as silero-vad's own Python
wrapper does. `vad.rs` keeps the tail of each chunk and zeroes it on `reset()`.

This is not optional and it fails silently. The input shape is dynamic, so a
bare 512 runs without error and reports no speech under any condition —
endpointing never fires, no turn ever completes, and nothing in the logs says
why. Any test asserting only "silence reads as silence" passes against a VAD in
that state, which is how it went unnoticed. `speech_reads_as_speech` in `vad.rs`
is the test that catches it, and it needs the real speech fixture at
`corpus/speech-16k.wav`.

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
