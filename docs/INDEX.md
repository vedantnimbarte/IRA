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
| [0007](decisions/0007-wingman-over-http-not-as-a-library.md) | Wingman over HTTP, not as a library dependency | proposed |
| [0008](decisions/0008-rejected-assembling-ira-from-echo-and-wingman.md) | Assembling IRA from Echo and Wingman | rejected |

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

Four decisions are unresolved and each blocks a phase. They are listed with
recommendations in [ROADMAP.md](ROADMAP.md#open-questions). The two worth knowing
about immediately:

- **Proactive speech.** When a background job finishes, IRA must speak with no wake
  word for the first time. Blocks P8.
- **kortex-memory is an MCP server**, so memory arrives at P4 through the adapter
  rather than being built into IRA. Its sixteen tool schemas may be more context
  than a voice turn should carry, which P4 has to decide.
