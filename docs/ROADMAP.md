# IRA Roadmap to v1.0

Internal engineering plan. IRA is a working duplex voice prototype — wake word,
endpointing, STT, streaming LLM, streaming TTS, barge-in. This takes it from a
prototype that answers one question to a product someone else can run.

Companion documents: [PRD.md](PRD.md) (what and why),
[ARCHITECTURE.md](ARCHITECTURE.md) (how, at system level),
[SPEC.md](SPEC.md) (how, at code level),
[TEST-PLAN.md](TEST-PLAN.md) (how we know).

## Where it stands

The loop runs end to end. The release binary opens the mic at 48 kHz stereo,
downmixes to 16 kHz mono, loads three openWakeWord models plus Silero VAD, and
spawns Piper in under half a second. Both backends are swappable by environment
variable, so STT can run entirely offline.

| Stage | Implementation | State | Evidence |
|---|---|---|---|
| Capture | cpal, downmix, linear resample | Working | Ratio + passthrough tests |
| Wake | openWakeWord, 3-stage chain | Working | Real-weights shape test |
| Endpoint | Silero VAD v5, 512-sample chunks | Working | Recurrent-state carry test |
| STT | Groq Whisper, or local whisper.cpp | Working | WAV accepted by local engine |
| LLM | Anthropic, or any OpenAI-compatible | Working | Both wire formats tested |
| TTS | Piper subprocess → rodio | Working | Manual only |
| Barge-in | Shared `CancellationToken` | **1 race** | Manual only |
| Failure paths | Log only | **Silent** | — |
| Tools | None | Absent | — |
| Timing | None | Absent | — |

### Three problems found by reading the code

**Failures are silent — `main.rs:219`.** `Turn::Failed` calls
`tracing::error!` and nothing else. When STT or the LLM fails the user hears no
sound at all, indistinguishable from thinking, from a missed wake word, or from a
crash. They repeat themselves, which triggers barge-in, which kills the next turn
too.

**A cancelled turn can still speak — `main.rs:184`.** After barge-in, `cancel`
is replaced with a fresh token. A sentence still buffered from the killed turn
passes the `!cancel.is_cancelled()` check against the *new* token and is spoken.
The window is narrow — the loop drains the channel while Listening — but it is
real.

**The prompt promises a screen that does not exist — `llm.rs:30`.** The system
prompt instructs the model, when an answer needs more room, to "say you have put
the detail on screen." IRA has no display. P5 makes it true; until then the clause
must go.

### The one that mattered most, found while fixing the others

**Endpointing never worked.** `vad.rs` fed Silero v5 a bare 512-sample chunk
where the model expects 576 — the 64 samples of preceding context that
silero-vad's own wrapper prepends. The input shape is dynamic, so it ran without
error and returned a probability near zero for *every* chunk, including
full-scale speech. `heard_speech` therefore never became true, the no-speech
timeout always won, and **no turn could ever complete**. Fixed in P1, with a test
that plays real speech; see [BASELINE.md](BASELINE.md) for the first working
measurements.

### And one gap that shapes the whole plan

There is not a single `Instant::now()` in `main.rs`. The project's stated purpose
is to find out whether talking to it *feels right*, and no part of that feeling
is measured. `ENDPOINT_MS` and `BARGE_IN_MS` are tuned by ear against no numbers.
That is why P0 is measurement and nothing else.

## Phases

Numbered because the order carries real information: each phase's exit criteria
are the next one's entry conditions. The exception is P6, which touches only
`audio.rs` and can run in parallel with P3–P5.

### P0 — Instrument the loop — *done*

Make "feels right" a number instead of an opinion. Baseline recorded in
[BASELINE.md](BASELINE.md).

- Timestamps threaded through one turn: wake fired → endpoint → transcript back
  → first LLM token → first audio sample out
- One structured log line per turn, with stage deltas
- `ira doctor`: models present, mic opens, keys set, whisper reachable
- A file-based frame source so the loop is testable at all (see
  [TEST-PLAN.md](TEST-PLAN.md))

**Exit:** a baseline latency table committed to the repo, covering both STT
backends.

**Why first:** every later phase claims a latency improvement. Without a baseline
those claims cannot be checked, and the tuning constants stay guesses.

### P1 — Close the honesty gaps — *done*

No state where IRA fails without saying so, or claims something untrue.

- Speak on `Turn::Failed` — one canned line, no LLM round trip
- Fix the stale-sentence race: carry the turn's token with each sentence
- Measure `BARGE_IN_GRACE_MS` from first audio out, not turn start — today STT
  and LLM latency spend the window before TTS ever speaks
- Remove the "on screen" clause until P5 makes it true

**Exit:** every failure path produces audible output; no cancelled turn can speak.
**Depends:** P0, for the grace-window fix.

### P2 — Conversation shape

Stop making the user say a name before every sentence.

- Follow-up window: after a reply completes, stay in Listening ~2 s with no wake
  word required
- Tune the window against P0 numbers, not by feel alone

**Exit:** a three-turn exchange conducted with one wake word.

Cheapest item on the roadmap — one state transition — and among the most
noticeable.

### P3 — Tool foundation

Tools stop being hypothetical; one works end to end.

- `Tool` trait, `ToolSpec`, `ToolOutcome`, registry
- Tool-calling in `llm.rs` for both wire formats — the `delta_text` seam is where
  it branches
- Filler speech for `Slow` tools
- New `Confirming` state for `mutates` tools

**Exit:** memory recall working end to end, including one confirmed write.
**Depends:** P1. `Confirming` is a state-machine change and should not be stacked
on a known race.
**Risk:** spoken sentences cannot be unspoken, so the prompt must enforce
speak-or-call, never both mid-sentence.

### P4 — MCP adapter

New tools arrive as config, not code.

- One adapter implementing `Tool`, lifting `wingman-mcp` (rmcp, stdio +
  Streamable-HTTP)
- `ira.toml` server list, shaped like Wingman's so entries copy between them
- Description truncation; `mutates` resolved from our config only

**Exit:** a third-party calendar server works with zero IRA code changes.
**Depends:** P3.

### P5 — Companion UI

Long answers and tool output get somewhere to land.

- Decide the surface: Tauri (Echo is the in-house precedent) or a lightweight
  always-on-top overlay
- Live transcript, tool results, long-form answers, diagrams
- Restore the "on screen" clause — now true

**Exit:** a long answer is summarised aloud in two sentences and rendered in full.
**Depends:** P3 — tool results are most of what it displays.

### P6 — Acoustic echo cancellation

Usable on speakers. Today, headphones are mandatory.

- `webrtc-audio-processing` wired into `audio.rs`
- Duck TTS on detected speech instead of hard-cutting, so interruption sounds
  deliberate

**Exit:** a full conversation held on speakers with no self-interruption.
**Depends:** nothing — touches only `audio.rs`, so it can run in parallel with
P3–P5.
**Risk:** the only item that may need real DSP iteration rather than integration.
Schedule it early against a second developer, not late against a deadline.

### P7 — Turn quality

Stop cutting people off mid-thought; cut a stage from the critical path.

- Streaming STT — send audio as it arrives instead of after endpointing
- Semantic endpointing (smart-turn v2) alongside VAD, so a thinking pause is not
  a turn end
- Train a real "IRA" wake word — a Colab run against the same three-stage chain;
  only the classifier `.onnx` changes

**Exit:** measured improvement against the P0 baseline, not a subjective one.
**Depends:** P0.

### P8 — Background jobs and Wingman

Tools that take minutes, not seconds.

- `Started(JobId)` plumbing; a `HashMap<JobId, JoinHandle>` is enough
- Proactive speech on completion — IRA speaks with no wake word for the first time
- Wingman via `serve` HTTP or as a library dependency (decide here, see
  [0007](decisions/0007-wingman-over-http-not-as-a-library.md))

**Exit:** a coding task issued by voice, reported when it finishes.
**Depends:** P3, P4, and the proactive-speech decision.

### P9 — v1.0 hardening

Something another person can install and run.

- Cross-platform: the Piper path, `.exe` assumptions and the PowerShell fetch
  script are all Windows-bound
- Packaging and signing (Echo's notes on unsigned-binary friction apply directly)
- Persistent transcript and session history
- Per-turn token and cost accounting

**Exit:** a clean machine goes from download to conversation without reading
source.

## Latency budget

Current constants and the one set of measurements taken so far. Everything in the
Measured column came from an 8-core CPU with integrated graphics against 2.8 s of
speech; the GTX 1650 that will actually run IRA is unmeasured. P0 replaces this
table with real per-stage numbers.

| Parameter | Value | Meaning | Failure mode if wrong |
|---|---|---|---|
| `ENDPOINT_MS` | 700 | Silence that ends your turn | Low: cuts you off. High: sluggish. |
| `BARGE_IN_MS` | 250 | Speech that counts as interrupting | Low: a cough stops her. High: laggy. |
| `BARGE_IN_GRACE_MS` | 300 | Ignore barge-in after taking the floor | Measured from the wrong moment — P1. |
| `NO_SPEECH_TIMEOUT_MS` | 3000 | Wake with no speech after it | Low: drops slow starters. |
| `MAX_UTTERANCE_MS` | 20000 | Hard cap on one utterance | — |
| VAD chunk | 512 / 32 ms | Silero v5 fixed window | Model requirement, not tunable. |
| Wake chunk | 1280 / 80 ms | openWakeWord step | ~2.2 s to fill the window at start-up. |
| Pre-roll kept | 400 ms | Audio retained from before wake | Low: loses the first word. |
| STT — tiny.en, 8 threads | ~750 ms | Measured, CPU | Drops proper nouns. |
| STT — base.en, 8 threads | ~1520 ms | Measured, CPU | Sluggish in the loop. |
| STT — Groq turbo | unmeasured | Needs a key | Network-dependent. |
| Piper voice load | 480 ms | Once at start-up | — |
| Piper respawn | ~250 ms | Paid only on barge-in | Delays her next reply. |

## Open questions

| Question | Why it matters | Blocks |
|---|---|---|
| When a background job finishes, how does IRA tell you? | Speaking unprompted is a capability IRA has never had. Recommendation: earcon at completion, spoken summary when next Idle. | P8 |
| Does kortex-memory exist? | `main.rs:212` names it as the replacement for the fixed 8-turn window, but there is no such repository on this machine. Either it lives elsewhere, or P3's memory tool begins by building it. | P3 scoping |
| Wingman as a library, or over HTTP? | A library dependency pulls 16 crates in for one call site. Recommendation: HTTP first. | P8 |
| Which UI framework? | Tauri is the in-house precedent from Echo; an overlay is lighter and more glanceable. | P5 |

## Considered and deferred

| Not doing | Reason |
|---|---|
| Dynamic-library plugin system | Echo has one and its own `PLUGINS.md` states the permissions are advisory and unenforced. MCP gives out-of-process isolation without a second extension mechanism. |
| Intent classifier / router | Tool-calling already picks the tool. A classifier is a second thing to keep in sync with the tool list. |
| Job queue or database | A `HashMap<JobId, JoinHandle>` covers background jobs until it demonstrably does not. |
| Further LLM provider abstraction | Already solved by `IRA_LLM_URL`. A provider trait on top would be a layer over a working URL. |
| Local LLM on the 1650 | 4 GB VRAM. A Q4 7B is ~4 GB alone and would contend with whisper for the same memory. |
| Replacing the linear resampler | Aliasing is inaudible on speech and Whisper does not care. Revisit only if word-error-rate measures worse than a reference resample. |
| Multi-language | English-only models are smaller and more accurate. Reconsider after v1.0. |

## Risks

| Risk | Exposure | Mitigation |
|---|---|---|
| AEC does not converge | P6 is the only phase needing DSP work rather than integration; failure means headphones forever | Start early and in parallel; press-to-talk is the fallback and sidesteps it entirely |
| Tool calls blow the latency budget | Every tool adds an LLM round trip before IRA can speak | P0 baseline first; filler speech masks it; reject tools exceeding budget at registration |
| Hostile or careless MCP server | Descriptions are an instruction channel; a bad `mutates` bypasses confirmation | Truncate descriptions; resolve `mutates` from our config, defaulting to true |
| GPU path unverified | Pack selection is tested only on its negative branch; no CUDA build has been run | First task on the 1650: run the pack and record numbers into the P0 table |
| Scope drift into Wingman | Wingman is a coding agent; its transcript is not speakable and its verification gate runs builds | Keep it behind `Started(JobId)` as one tool among many, never as IRA's brain |
