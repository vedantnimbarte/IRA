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

### P2 — Conversation shape — *done*

Stop making the user say a name before every sentence.

- Follow-up window: after a reply completes, stay in Listening ~2 s with no wake
  word required
- Tune the window against P0 numbers, not by feel alone

**Exit:** a three-turn exchange conducted with one wake word.

Cheapest item on the roadmap — one state transition — and among the most
noticeable.

### P3 — Tool foundation — *done*

Tools stop being hypothetical; one works end to end.

- `Tool` trait, `ToolSpec`, `ToolOutcome`, registry
- Tool-calling in `llm.rs` for both wire formats — the `delta_text` seam is where
  it branches
- Filler speech for `Slow` tools
- New `Confirming` state for `mutates` tools

**Exit:** a tool called and answered end to end, and a mutating tool confirmed.
Memory moved to P4 once kortex-memory turned out to be an MCP server — see the
open questions below. The clock ships instead: the one tool worth keeping
in-process rather than spawning a subprocess to read.
**Depends:** P1. `Confirming` is a state-machine change and should not be stacked
on a known race.
**Risk:** spoken sentences cannot be unspoken, so the prompt must enforce
speak-or-call, never both mid-sentence.

### P4 — MCP adapter — *done*

New tools arrive as config, not code.

- One adapter implementing `Tool`, lifting `wingman-mcp` (rmcp, stdio +
  Streamable-HTTP)
- kortex-memory as the first server, which delivers FR-15 (recall across
  sessions) — it speaks both stdio and HTTP/SSE
- `ira.toml` server list, shaped like Wingman's so entries copy between them
- Description truncation; `mutates` resolved from our config only

**Exit:** a third-party calendar server works with zero IRA code changes.
**Depends:** P3.

**Delivered:** the adapter, `ira.toml`, per-tool policy, `only` selection,
description truncation, and graceful degradation when a server is missing,
broken or slow. Verified against a stub MCP server, including a tool the server
described as harmless that IRA asked about anyway.

**Not delivered: FR-15.** kortex-memory has never been connected. It needs
Postgres + pgvector, Redis and MinIO running, which is a deployment question
rather than a code one. The adapter is ready for it; nothing has proved the pair
work together.

### P5 — Companion UI — *done*

Long answers and tool output get somewhere to land.

- Surface decided: a page IRA serves on loopback, read in a browser. Not Tauri,
  not a GUI crate — see [0009](decisions/0009-the-screen-is-a-served-page.md)
- Live transcript, tool calls and results, full replies, per-turn timings
- The "on screen" clause is restored, but only while a page is connected: IRA
  counts its watchers and will not promise a screen nobody is looking at

**Exit:** a long answer is summarised aloud in two sentences and rendered in full.
**Depends:** P3 — tool results are most of what it displays.

**Not verified:** that the model actually keeps to two sentences when it knows
there is a screen. That is the prompt's job and needs a real model; every run so
far used a stub that says whatever it is told to.

### P6 — Acoustic echo cancellation — *partly done*

Usable on speakers. Today, headphones are mandatory.

- ~~`webrtc-audio-processing` wired into `audio.rs`~~ **deferred** — see
  [0010](decisions/0010-press-to-talk-before-echo-cancellation.md)
- Press-to-talk: `IRA_PTT=1` disarms voice barge-in, `POST /talk` takes the
  floor or interrupts. Speakers work today, without hands-free interruption
- Duck TTS to 35 % at the first hint of speech, cutting only once the
  interruption is confirmed

**Exit:** a full conversation held on speakers with no self-interruption.
Reachable now with `IRA_PTT=1`; not yet reachable hands-free.

**Why echo cancellation is not here.** Two findings, both from trying it:
`webrtc-audio-processing` builds its bundled C++ through meson and ninja, which
turns `cargo build` into a multi-toolchain build and fights P9's exit criterion.
And echo cancellation is a property of a room — a speaker, an acoustic path, a
microphone — so no replayed WAV can verify it. Shipping unverified DSP into the
files [0001](decisions/0001-audio-path-stays-in-one-process.md) closes would
degrade capture silently. There is also an unsolved precondition: the far-end
signal must be time-aligned with capture, and rodio does not report output
timing.

**Fixed along the way.** A long reply was being abandoned before it spoke.
`Tts::idle()` could not tell "finished speaking" from "piper has not started
yet": `say()` stamps the audio clock, so 400 ms later an empty queue looked
idle, and piper needs a second or more to synthesise a long sentence. The
longer the answer, the more certain the failure — which is the wrong way round,
and it silently broke the long answers P5 had just shipped. `idle()` now waits
for what it asked for, bounded so a dead piper cannot strand the loop.

### P7 — Turn quality — *partly done*

Stop cutting people off mid-thought; cut a stage from the critical path.

- **Speculative transcription.** Endpointing spends `ENDPOINT_MS` proving the
  user stopped, and transcription then takes about as long again; run in
  sequence that is the whole latency budget. Transcription now starts on a
  *pause* (`SPECULATE_MS`, 200 ms) rather than on proof the turn is over. If the
  pause turns out to have been mid-thought the guess is discarded and remade,
  which costs idle CPU and no wall-clock. Measured saving: **522 ms**, see
  [BASELINE.md](BASELINE.md).
- ~~Semantic endpointing (smart-turn v2)~~ **deferred.** A heuristic over the
  speculative transcript was tried on paper and rejected: whisper punctuates
  short fragments confidently, so "What was that thing?" reads as complete and
  T-2 would still fail. Doing it properly means a second model whose accuracy
  cannot be judged without real speech, and a wrong turn-detector *extends every
  turn* — a latency regression on top of the win above.
- ~~Train a real "IRA" wake word~~ **deferred.** A Colab training run, then a
  false-wake rate that can only be measured in a room over hours.

**Exit:** measured improvement against the P0 baseline, not a subjective one.
**Met**, by an interleaved same-machine A/B rather than a comparison against the
older baseline: median `total_ms` 1667 → 1145. The saving lands on
`ENDPOINT_MS - SPECULATE_MS`, which is what the design predicted.

**Depends:** P0 — and this is what P0 was for. The change is invisible without
the `turn` line, and could not have been argued for without a number.

### P8 — Background jobs and Wingman — *done*

Tools that take minutes, not seconds.

- `Started(JobId)`: a tool declaring `latency = "background"` is detached by the
  host and answered for immediately, so the loop takes the next turn while it
  runs. Its cancellation token is its own — interrupting the sentence that asked
  is not a reason to abandon work already agreed to
- Proactive speech: a pip the instant a job lands, the words at the next `Idle`.
  See [0011](decisions/0011-a-tone-now-and-words-when-idle.md)
- Jobs lost at shutdown are counted and said out loud
- Wingman, as `src/wingman.rs`: one tool over `wingman serve`'s HTTP API,
  registered only when `IRA_WINGMAN_URL` names a daemon that answers. It is an
  MCP *client*, not a server, so it is the one capability that cannot arrive as
  a line of `ira.toml`. See
  [0012](decisions/0012-wingman-is-a-built-in-not-an-mcp-shim.md)

**Exit:** a coding task issued by voice, reported when it finishes. **Met**,
against a stub of Wingman's HTTP API: IRA asked "Send that to Wingman?", heard
"Yes", POSTed `add a retry to the uploader` to
`/v1/projects/ira/turns`, said "I have sent that to Wingman" without waiting,
returned to Idle, and reported the summary when the turn landed. Saying "No"
sent nothing at all, and a 429 was reported as a refusal rather than as
silence. Not met against `wingman serve` itself, which has never been run.

**Depends:** P3, P4, and the proactive-speech decision — which
[0011](decisions/0011-a-tone-now-and-words-when-idle.md) now settles.

### P9 — v1.0 hardening — *partly done*

Something another person can install and run.

- Cross-platform: the Piper binary name is chosen by platform, `ira doctor`
  names the setup script that exists on the machine it is running on, and
  `scripts/fetch-models.sh` is the POSIX counterpart of the PowerShell one
- Persistent transcript: JSONL, one object per turn, `IRA_TRANSCRIPT` to move
  or disable it
- Token accounting: reported when the endpoint volunteers a count
- ~~Packaging and signing~~ **deferred** — release infrastructure and a
  certificate, neither of which exists yet. Echo's notes on unsigned-binary
  friction apply unchanged

**Exit:** a clean machine goes from download to conversation without reading
source. **Unproven.** Every Windows-bound assumption found in the source has
been removed, but IRA has never been built or run anywhere except this machine.
The POSIX script parses and its model-fetching path was exercised; the Piper
download and the whisper.cpp build in it have not been run, because both need a
Linux or macOS box.

**Cost, not just tokens.** Deliberately not implemented. Pricing means a table
of per-model rates that goes stale silently and is wrong in the direction of
under-reporting. Tokens are what IRA can know for certain; anything wanting
currency can multiply them by a number it maintains itself.

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
| Where does kortex-memory run? | Attempted. The one-container image (`make local-build && make local-run`) is the right path — Postgres, Redis, the API, the MCP server and the worker in one container. The build reached its final layer three times and the Docker Desktop engine died each time, ending at "Docker Desktop is unable to start". The blocker is that machine's Docker, not kortex and not IRA. | FR-15 |
| ~~Sixteen schemas in a voice turn?~~ **Solved.** | `only = [...]` per server. Every schema is sent on every round and a tool-calling turn has two rounds, so exposing all sixteen would put thirty-two schemas in front of the model per turn. | ~~P4~~ |
| ~~Wingman as a library, or over HTTP?~~ **Answered: HTTP.** | A library dependency pulls 16 crates in for one call site, and the process boundary is what lets a coding turn outlive the conversation that asked for it. Connected and verified against a stub of that API, not against `wingman serve`. | ~~P8~~ |
| ~~Which UI framework?~~ **Answered: none.** | A page served on loopback, read in a browser. No dependency, no second build, and the browser supplies scrolling, selection and theming. An overlay remains possible later and consumes the same event stream. | ~~P5~~ |

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
