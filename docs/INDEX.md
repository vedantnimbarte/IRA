# IRA Documentation Index

IRA is a duplex voice loop: wake word → VAD endpointing → STT → streaming LLM →
streaming TTS, with barge-in. These documents cover where it stands and what it
takes to reach v1.0.

Start with the [README](../README.md) for how to run it.

## Read in this order

1. **[PRD.md](PRD.md)** — what IRA must do and how we will know it does it.
   The problem, product principles, 24 functional and 9 non-functional
   requirements, release gates, non-goals.
   - Read this if: you are deciding what to build, or arguing about scope.
   - Does not contain: any implementation detail.

2. **[ROADMAP.md](ROADMAP.md)** — ten phases from prototype to v1.0.
   Current state with evidence, the three known defects, phase-by-phase work with
   exit criteria and dependencies, the latency budget, risks.
   - Read this if: you want to know what happens next and in what order.

3. **[ARCHITECTURE.md](ARCHITECTURE.md)** — how the system is put together.
   The single-process audio constraint and why it exists, the pipeline, the state
   machine, the module map, the tool subsystem, the threat model.
   - Read this if: you are extending IRA, or wondering why something is shaped the
     way it is.

4. **[SPEC.md](SPEC.md)** — enough detail to implement without re-deciding
   anything. Types, the exhaustive state transition table, the error-to-sound
   table, every environment variable, the `ira.toml` schema, the instrumentation
   contract, per-phase acceptance criteria.
   - Read this if: you are writing the code.

5. **[BASELINE.md](BASELINE.md)** — the measured latency baseline, and an
   explicit list of what is still unmeasured.
   - Read this if: you are about to claim something got faster.

6. **[TEST-PLAN.md](TEST-PLAN.md)** — how we know it works.
   The coverage gap and how to close it, the corpus, benchmark method, manual test
   scripts, fault injection, CI gates.
   - Read this if: you are about to call a phase done.

## Decisions

Architecture decision records live in [decisions/](decisions/). Each states the
context, the decision, its consequences, and what would change it.

| ADR | Decision | Status |
|---|---|---|
| [0001](decisions/0001-audio-path-stays-in-one-process.md) | The audio path stays in one process | accepted |
| [0002](decisions/0002-tools-behind-a-trait-mcp-via-one-adapter.md) | Tools sit behind a Rust trait, with MCP through one adapter | accepted |
| [0003](decisions/0003-stt-and-model-are-urls.md) | STT and the model are URLs, not code paths | accepted |
| [0004](decisions/0004-write-status-is-ours-not-the-servers.md) | Write status is declared by us, never by the server | accepted |
| [0005](decisions/0005-slow-tools-get-filler-speech.md) | Slow tools get filler speech | accepted |
| [0006](decisions/0006-background-jobs-return-an-id.md) | Background jobs return an id; no queue, no database | accepted |
| [0007](decisions/0007-wingman-over-http-not-as-a-library.md) | Wingman over HTTP, not as a library dependency | accepted |
| [0008](decisions/0008-rejected-assembling-ira-from-echo-and-wingman.md) | Assembling IRA from Echo and Wingman | rejected |
| [0009](decisions/0009-the-screen-is-a-served-page.md) | The screen is a page IRA serves, not a window it owns | accepted |
| [0010](decisions/0010-press-to-talk-before-echo-cancellation.md) | Press-to-talk ships before echo cancellation | accepted |
| [0011](decisions/0011-a-tone-now-and-words-when-idle.md) | A tone now, and words when IRA next has the floor | accepted |
| [0012](decisions/0012-wingman-is-a-built-in-not-an-mcp-shim.md) | Wingman is a built-in, not an MCP shim | accepted |

## Looking for something specific?

| I want to… | Go to |
|---|---|
| Run IRA | [README](../README.md) |
| Set an environment variable | [SPEC.md](SPEC.md#environment-variables) |
| Run STT offline | [README](../README.md) — Local STT |
| Use OpenRouter or a local model | [README](../README.md) — OpenRouter |
| Understand barge-in | [ARCHITECTURE.md](ARCHITECTURE.md) — the governing constraint |
| Add a tool | [ARCHITECTURE.md](ARCHITECTURE.md) — tool subsystem, then [SPEC.md](SPEC.md) — types |
| Know what a phase must satisfy | [SPEC.md](SPEC.md) — acceptance criteria |
| Know why we did not do X | [ROADMAP.md](ROADMAP.md) — considered and deferred, or [decisions/](decisions/) |
| Measure latency | [TEST-PLAN.md](TEST-PLAN.md) — method, then [BASELINE.md](BASELINE.md) |
| Replay a WAV instead of using the mic | [SPEC.md](SPEC.md#environment-variables) — `IRA_AUDIO_FILE` |

## Open questions

The design questions the roadmap opened have all been answered, each in a
decision record under [decisions/](decisions/). What remains open is not a
decision but a measurement — four things nobody has checked, listed in the
README under *What has not been verified* and in
[TEST-PLAN.md](TEST-PLAN.md):

- **A live microphone.** Every acoustic behaviour — barge-in, the wake word,
  ducking, press-to-talk — has only been exercised by replaying WAV files.
- **A real model.** Time-to-first-token has never been observed above zero, so
  the latency work in [BASELINE.md](BASELINE.md) optimised the half of the
  budget that could be measured.
- **Another machine.** IRA has only ever been built and run on one Windows box.
- **kortex-memory and Wingman.** Both adapters are built and tested against
  hostile stubs; neither real integration has been connected.
