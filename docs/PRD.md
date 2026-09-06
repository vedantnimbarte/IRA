# IRA Product Requirements

What IRA must do and how we will know it does it. Not how it is built — see
[ARCHITECTURE.md](ARCHITECTURE.md) and [SPEC.md](SPEC.md) for that.

**Status:** draft for review
**Baseline:** working prototype, 1,061 lines
**Horizon:** v1.0

## The problem

Voice assistants fail at conversation, not at recognition. Speech-to-text has
been good enough for years; what remains broken is turn-taking. They cut you off
when you pause to think. They cannot be interrupted — you wait out an answer you
already know is wrong. They require a wake word before every single sentence,
which makes a three-turn exchange feel like operating a machine rather than
talking.

These are latency and turn-taking problems, not intelligence problems. A slower
model that yields the floor correctly beats a smarter one that talks over you.
IRA exists to get that part right first, on the premise that if turn-taking is
wrong, no amount of tools, memory or UI will rescue it.

**The bet:** duplex turn-taking — speak, be interrupted, yield instantly, resume
— is the feature, and everything else is table stakes. If that bet is wrong, the
product is just another chat box with a microphone.

## Who it is for

| User | Context | What they need | By |
|---|---|---|---|
| Primary — the owner | Windows desktop, GTX 1650, headphones, works at the machine all day | Hands-free answers and actions without breaking focus or reaching for a keyboard | Now → P8 |
| Secondary — a second installer | Someone else's machine, no build toolchain, no patience for source | Download, run, talk. Diagnosis when it fails rather than silence | P9 |
| Non-user — a bystander | Same room, talking to someone else, TV on | Not to trigger it by accident, and for it not to record them | P7 |

The primary user is the only one who matters before P9. That is a deliberate
scoping choice: designing for a hypothetical second user now would push work
toward packaging and cross-platform support and away from turn-taking, which is
the thing actually in doubt.

## Product principles

Decision rules, not aspirations. Where a trade-off appears in implementation,
these settle it.

1. **Always interruptible.** If the user starts speaking, IRA stops. No
   exceptions, no minimum reply length, no "let me finish this sentence."
   Interruption latency is a hard requirement.
2. **Two sentences, never a paragraph.** Spoken answers are not skimmable.
   Brevity is a functional constraint, not a style preference. Long content goes
   on screen with a one-line spoken summary.
3. **Never silent on failure.** Every failure produces audible output. Silence is
   indistinguishable from thinking, a missed wake word, or a crash — and it makes
   the user repeat themselves, which breaks the next turn too.
4. **Local where it is cheap.** Wake word, endpointing and speech run on-device
   because they can. STT is local when latency allows. The model stays hosted
   because that is the one stage where local measurably hurts on 4 GB of VRAM.
5. **Confirm before irreversible.** Voice is a lossy channel. A misheard sentence
   must not send an email. Reads run freely; anything that mutates state asks
   first, aloud.
6. **Measured, not asserted.** "Feels right" is a number or it is an opinion. No
   latency claim ships without a measurement against a fixed corpus.

## Functional requirements

Priority is **Must** for v1.0, **Should** for v1.0 if time allows, **Could** for
after. **Shipped** items already work.

### Conversation

| ID | Requirement | Priority | Phase |
|---|---|---|---|
| FR-1 | Activate hands-free on a spoken wake word, with audible acknowledgement | Shipped | — |
| FR-2 | Detect end of the user's turn from silence and respond without further prompting | Shipped | — |
| FR-3 | Stop speaking within the interruption budget when the user begins talking, and treat what they said as the next turn | Shipped | — |
| FR-4 | Begin speaking before the model has finished generating the reply | Shipped | — |
| FR-5 | Accept a follow-up turn without requiring the wake word again | Shipped | — |
| FR-6 | Not end the user's turn on a mid-thought pause | Should | P7 |
| FR-7 | Offer press-to-talk as an alternative activation mode | Should | P6 |
| FR-8 | Respond to its own name rather than a placeholder wake word | Should | P7 |

### Reliability and feedback

| ID | Requirement | Priority | Phase |
|---|---|---|---|
| FR-9 | Say something audible on every failure, naming what the user should do | Must | P1 |
| FR-10 | Never speak content from a turn the user already interrupted | Must | P1 |
| FR-11 | Never claim to have done something it cannot do | Must | P1 |
| FR-12 | Diagnose its own setup on demand: models, microphone, keys, local engines | Must | P0 |
| FR-13 | Keep a readable transcript of past conversations | Should | P9 |
| FR-14 | Report what each turn cost in tokens | Could | P9 |

### Acting, not just answering

| ID | Requirement | Priority | Phase |
|---|---|---|---|
| FR-15 | Recall facts from earlier conversations, not only the current session | Must | P4 — adapter ready, kortex-memory not yet connected |
| FR-16 | Speak a holding phrase while a slow action runs, rather than going quiet | Must | P3 |
| FR-17 | Ask for spoken confirmation before any action that changes state, and abort on refusal | Must | P3 |
| FR-18 | Gain new capabilities by configuration, without a code change or rebuild | **Shipped** | P4 |
| FR-19 | Accept a task that takes minutes, and report back when it finishes | Should | P8 |
| FR-20 | Dictate text into whichever application has focus | Could | P8 |

### Screen

| ID | Requirement | Priority | Phase |
|---|---|---|---|
| FR-21 | Show a live transcript of the current conversation | **Shipped** | P5 |
| FR-22 | Render long answers in full while speaking a one-line summary | **Shipped** | P5 |
| FR-23 | Show tool results that do not read aloud well — lists, tables, code | **Shipped** | P5 |
| FR-24 | Stay out of the way when not in use | **Shipped** | P5 — a tab takes no focus |

## Non-functional requirements

| ID | Requirement | Target | Notes |
|---|---|---|---|
| NFR-1 | End of user speech → first audio from IRA | < 1200 ms p50, < 2000 ms p95 | The number that decides whether it feels conversational |
| NFR-2 | User starts speaking → IRA silent | < 400 ms | Includes VAD confirmation window; the audio queue clears instantly |
| NFR-3 | Wake word spoken → acknowledgement chirp | < 500 ms | Below this, the user starts talking before the cue |
| NFR-4 | False wake events while idle | < 1 / day | Measured in a room with normal conversation and audio playing |
| NFR-5 | Turns completed without the user repeating themselves | > 95 % | The single best proxy for whether turn-taking works |
| NFR-6 | Operates with no network reachable | STT + wake + VAD + TTS | The model is the only stage requiring connectivity |
| NFR-7 | Audio leaving the machine | Only when cloud STT is configured | Local STT is a supported configuration, not a future one |
| NFR-8 | Cold start → ready for the wake word | < 3 s | Model loading dominates; measured at 1.5 s today |
| NFR-9 | Runs within the target machine's VRAM | ≤ 4 GB total | Constrains STT model size and rules out a co-resident local LLM |

**These targets are proposals, not measurements.** Nothing in the loop is
instrumented today, so every number above is a considered guess. P0 replaces them
with measured values against a fixed corpus. Expect NFR-1 in particular to move
once local STT latency is known on the actual GPU.

## How we will know it works

Two gates. The quantitative gate is NFR-1 through NFR-5 measured against a fixed
audio corpus, and it is necessary but not sufficient — a system can hit every
number and still feel wrong. The qualitative gate decides the product:

- **The dogfood test.** The owner uses IRA as the default way to ask a question
  for one full working week, without falling back to typing out of frustration.
- **The interruption test.** A stranger, given no instructions beyond "talk to
  it," interrupts it within the first three turns without being told they can. If
  interruption is not discoverable, it is not a feature.
- **The pause test.** A user thinking mid-sentence — "what was that thing… the one
  from Tuesday" — is not cut off.

The dogfood test is the release gate for v1.0. The other two are diagnostics that
tell us which phase to invest in next.

## Non-goals

| Not building | Why not |
|---|---|
| A general chat interface | Text chat already exists and is better at text. IRA earns its place only when speaking is genuinely better than typing. |
| A coding agent | Wingman is that, and it stays a tool IRA can call rather than something IRA becomes. Its output is a transcript, not an answer. |
| Multi-user or multi-tenant | One person, one machine, one microphone. Speaker identification is a different product. |
| Always-on cloud recording | Audio is buffered in memory for the length of one utterance and discarded. Nothing is stored for training. |
| Fully offline including the model | 4 GB VRAM. A quantised 7B is the entire budget and would contend with STT. Revisit on different hardware, not by compromising answer quality. |
| Phone or mobile client | The premise is hands-free at a desk you are already working at. Different constraints, different competitors. |
| Wake-word-free open microphone | Continuous listening with no trigger is a privacy posture we are not taking, and a false-positive problem we do not need. |

## Open product questions

| Question | Why it matters | Needed by |
|---|---|---|
| When a long task finishes, may IRA speak unprompted? | It has never spoken without a wake word. Interrupting the user is a different product than waiting quietly. Recommendation: a tone at completion, spoken summary when next idle. | P8 |
| ~~Window or overlay?~~ **Answered: a page.** | Something you visit, in a browser tab. It is browsable rather than glanceable, which is the right trade for long answers and tool output but not for a passing status. An overlay stays possible and would read the same event stream. | ~~P5~~ |
| What is IRA's personality? | Currently defined only by a brevity instruction. Two sentences is a constraint, not a voice, and the voice is most of what a user remembers. | P5 |
| ~~Where does memory live?~~ **Answered.** | kortex-memory, an MCP server with sixteen tools over stdio and HTTP/SSE. It is therefore a P4 integration, not a P3 built-in, and IRA does not implement memory itself. | ~~P3~~ |
