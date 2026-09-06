# IRA Test & Evaluation Plan

The loop's value is a subjective property measured objectively. This document
defines the corpus, the harness, the numbers and the manual scripts — and states
plainly which parts cannot be automated and must be judged by a person.

**Automated today:** 20 tests
**State machine coverage:** the barge-in predicate; transitions still need the replay harness
**Release gate:** one week of dogfood

## The coverage that is missing

The ten existing tests are well chosen: each protects something that fails
*silently* rather than loudly. Two run real ONNX weights and check the tensor
shapes threaded between openWakeWord's three stages and Silero's recurrent state.

| Component | What could break | Coverage |
|---|---|---|
| Resampler | Wrong ratio corrupts every downstream stage inaudibly | 2 tests |
| Wake chain | Tensor shape mismatch between the three models | 1 test |
| VAD | Recurrent state not carried → model forgets context each chunk | 1 test |
| VAD | Reports no speech ever, silently — see below | 2 tests |
| WAV encoding | Malformed container rejected by the STT engine | 2 tests |
| Sentence splitter | Decimals split → "three point" / "fifty" as two utterances | 3 tests |
| LLM wire formats | Reading one format's frames with the other's rules → IRA goes mute | 1 test |
| **State machine** | **Every turn-taking behaviour the product exists for** | Replay harness exists; transitions still uncovered |
| Barge-in | Interruption ignored, or a cancelled turn still speaks | 2 tests (the predicate) |
| TTS queue | Playback not cleared on interrupt; drain detection wrong | None |
| Failure paths | Silent failure — the current worst UX defect | None |

**The gap is exactly where the value is.** Everything with coverage is a pure
function. Everything without coverage is the state machine — which is the product.
It has already shipped one race, and that race was found by reading, not by
testing.

**This gap has already cost a working product once.** `vad.rs` fed Silero 512
samples where the model wants 576, so it reported no speech under any condition.
Endpointing never fired and no turn could complete. The existing VAD test
asserted only that silence reads as silence, which an always-false VAD satisfies
perfectly. A test that plays real speech and expects a "speech" verdict is the
only thing that distinguishes the two, and it now exists.

## Making the loop testable

The state machine is untested because it needs a microphone, and a microphone
needs a human. That is solvable: the loop does not care where frames come from. It
reads `Vec<f32>` at 16 kHz off a channel.

```bash
# An alternative frame source, gated by env var.
# audio.rs already produces exactly this shape.
IRA_AUDIO_FILE=corpus/barge-in-01.wav cargo run

# Frames fed at wall-clock rate so timing behaviour is real,
# or as fast as possible with a virtual clock for CI.
IRA_AUDIO_FILE=... IRA_CLOCK=virtual cargo test --test loop
```

This one addition makes wake detection, endpointing, barge-in timing, the
follow-up window and every confirmation path deterministic and CI-runnable. It is
a small change to `audio.rs` — a second implementation of the frame source — and
it unlocks more coverage than any other work on the plan.

**Build it at P0**, alongside instrumentation: both exist to make later phases
measurable, and both are cheap now and awkward to retrofit once the state machine
has grown a fourth state and a job table.

## The corpus

A fixed set of WAV files under `corpus/`, committed to the repo. Two sources, for
two different jobs.

| Source | Valid for | Not valid for |
|---|---|---|
| Piper-synthesised | Latency. Reproducible, zero-cost, regenerable from a text file, and latency depends on duration rather than voice quality. | Accuracy. Synthetic speech is unnaturally clean; word-error-rate measured on it will be optimistic and misleading. |
| Real recordings | Accuracy, false-wake rate, endpointing against natural pauses and disfluency. | Nothing — but it costs a person's time and cannot be regenerated on demand. |

**Synthetic audio cannot test the wake word.** openWakeWord is trained on real
speech and does not fire on Piper's output — measured, not assumed. Latency
replays therefore set `IRA_SKIP_WAKE` and start with the floor open, which is
sound because NFR-1 is endpoint-to-first-audio and does not involve the wake
word. NFR-3 and NFR-4 need real recordings and cannot be automated from a
synthetic corpus.

`corpus/speech-16k.wav` is committed: 1.96 s of speech at the model's own sample
rate, so the VAD test needs no resampling. It is the fixture that catches a dead
VAD.

Synthesising the latency corpus is already proven — Piper is in the repo and its
output round-tripped correctly through whisper.cpp during the STT work:

```powershell
# One line of text per utterance -> one WAV each.
Get-Content corpus/utterances.txt | ForEach-Object {
    $_ | .\piper\piper.exe --model models\en_US-amy-medium.onnx `
                           --output_file "corpus/$($i).wav"
}
```

### Required utterances

| ID | Content | Exercises |
|---|---|---|
| C-1 | Short command, ~1 s | Floor of the latency budget |
| C-2 | Question, ~3 s | The typical case; the number quoted as NFR-1 |
| C-3 | Long question, ~10 s | STT scaling with duration |
| C-4 | Contains proper nouns and place names | Where `tiny.en` is known to fail |
| C-5 | Contains prices and decimals | Sentence splitter, end to end rather than in isolation |
| C-6 | 900 ms pause mid-sentence | Endpointing — must *not* end the turn |
| C-7 | Wake word only, then silence | No-speech timeout returns to Idle quietly |
| C-8 | Speech beginning 400 ms after the wake word | Pre-roll retention — the first word must survive |
| C-9 | One hour of podcast, no wake word | False-wake rate for NFR-4 |
| C-10 | "yes" / "no" / "maybe later" | Confirmation grammar, including ambiguity failing closed |

## Benchmark method

The instrumentation contract in [SPEC.md](SPEC.md) emits one `turn` event per
turn. The harness replays the corpus, parses those lines, and reports the
distribution.

- **20 runs per utterance.** Enough for a stable p95 without making the run take
  longer than anyone will wait.
- **Report p50 and p95, never a mean.** One four-second turn does more damage than
  twenty nine-hundred-millisecond turns do good, and a mean hides exactly that.
- **Discard the first run** of each configuration. Model load and a cold page cache
  are start-up costs, not turn costs.
- **Record the configuration with the numbers** — STT backend, model, thread count,
  GPU pack, machine. A latency figure without them is not comparable to anything.
- **Both STT backends every time.** The local/cloud gap is a product decision, and
  it moves as models change.

> **Known-invalid baseline.** The only measurements taken so far — `tiny.en`
> ~750 ms, `base.en` ~1520 ms — came from an 8-core CPU with integrated graphics,
> against a single 2.8-second utterance, timed with `curl` rather than the
> harness. They are indicative, not a baseline. The GTX 1650 that will run IRA has
> never been measured, and no GPU pack has been executed.

## Manual test scripts

Run before each phase is called done. These require a person because they test
whether something feels right, which no assertion can express.

### T-1 — Interrupt mid-sentence
**Do:** Ask a question with a long answer. Once she is three or four words in,
start talking over her.
**Pass:** Audio stops before you finish your second word. Your interrupting words
become the next question, including the first one. She does not resume the
abandoned answer.
**Phase:** every phase. This is the regression test for the product's core claim.

### T-2 — Think out loud
**Do:** Say "what was that thing…", pause for a full second, then "…the one from
Tuesday".
**Pass:** One turn, containing the whole sentence. Currently expected to *fail* at
700 ms endpointing — that failure is what P7 fixes, and this script is how you know
it did.
**Phase:** P7

### T-3 — Refuse a confirmation four ways
**Do:** Trigger a mutating tool. Refuse once with "no"; once by staying silent;
once with "hmm, maybe" twice; once by talking over the question.
**Pass:** The action does not happen in any of the four. Ambiguity and interruption
both fail closed. Only an explicit yes executes.
**Phase:** P3

### T-4 — Pull the plug mid-turn
**Do:** Ask a question, then kill `whisper-server` (or drop the network) before the
answer starts.
**Pass:** You hear something within a second or two saying she could not do it.
Silence is a failure of this test.
**Phase:** P1

### T-5 — Take the headphones off
**Do:** Switch to speakers at normal listening volume. Hold a three-turn
conversation.
**Pass:** She never interrupts herself. Barge-in still works, and still meets its
budget with AEC in the path.
**Phase:** P6

### T-6 — Leave it running all day
**Do:** Idle in a room with normal conversation, music and video for eight hours.
**Pass:** At most one false wake. No memory growth. No crash. Record what triggered
any false wake — that recording becomes corpus material.
**Phase:** P7, P9

### T-7 — The stranger test
**Do:** Hand it to someone told only "talk to it". Say nothing about interrupting.
Watch.
**Pass:** They interrupt it within three turns without being told they could. If
interruption is not discoverable, it is not a feature — it is a setting nobody
finds.
**Phase:** P5, and again before v1.0

### T-8 — Ask for something slow, then walk away
**Do:** Issue a background task. Leave the room. Come back after it finishes.
**Pass:** The completion tone fired at the time it finished. The spoken summary is
delivered when you next engage, not shouted into an empty room, and not lost.
**Phase:** P8

## Fault injection

Each row of the spec's error table needs a way to be provoked on demand. Without
these, FR-9 is untestable and the error paths rot.

| Fault | How to provoke | Expected |
|---|---|---|
| STT unreachable | Point `IRA_STT_URL` at a closed port | "I didn't catch that." |
| STT empty result | Feed C-9 silence after a forced wake | Low double tone, back to Idle |
| Model unreachable | `IRA_LLM_URL` to a closed port | "Trouble thinking right now." |
| Model auth rejected | Invalid `IRA_LLM_KEY` | Startup refusal, not a turn-time failure |
| Wrong model id | `IRA_LLM_MODEL=nonsense` | Named error; the known OpenRouter footgun |
| Piper dies | Kill the child process mid-reply | Error earcon, respawn, next turn works |
| Stream truncated | Proxy that closes mid-SSE | Queued sentences finish; no apology appended |
| Tool hangs | Stub tool sleeping past its budget | Abandoned and reported aloud |
| MCP server absent | Config a command that does not exist | Logged at startup; boot succeeds; tool absent |
| Hostile description | Stub server with a 4 KB instruction-shaped description | Truncated at `DESC_MAX`, fenced, no behaviour change |
| Spoofed write flag | Stub server declaring `mutates: false` on a write | Still confirmed — our config decides |

## CI gates

CI has no microphone, no speaker and no API keys. What it can enforce is still most
of what matters, once the file-based frame source exists.

| Gate | Runs | Blocks merge |
|---|---|---|
| `cargo test` | Every push | Yes |
| `cargo clippy --all-targets` | Every push | Yes — currently clean, keep it there |
| `cargo fmt --check` | Every push | **Not yet** — the tree has ~13 pre-existing diffs; needs one formatting commit before this can gate |
| Loop tests via `IRA_AUDIO_FILE` | Every push, virtual clock | Yes, from P0 |
| Latency benchmark, local STT | Nightly on the GPU machine | No — report a trend; a threshold here would flake on shared hardware |
| Fault-injection suite | Every push | Yes, from P1 |
| Manual scripts T-1, T-3, T-4 | Before each phase sign-off | Yes, by hand |

Tests requiring a live service stay skipped-by-default and self-describing,
following the pattern already in the repo: the ONNX tests skip with a note when
`models/` is empty, and the local-STT test skips unless `IRA_STT_URL` is set. A
fresh clone passes without setup, and a configured machine runs more.

## The release gate

Everything above is necessary and none of it is sufficient. A system can satisfy
every number in this document and still be something you avoid using. So v1.0
ships on one criterion:

> **One week of dogfood.** The owner uses IRA as the default way to ask a question
> for five consecutive working days, without falling back to typing out of
> frustration. Every fallback gets logged with one line explaining why. Those lines
> are the real backlog — more informative than any metric here, because each one is
> a moment the product lost to a text box.

T-2 and T-7 are diagnostics rather than gates: they tell you *which* phase to
invest in next, when the dogfood week produces a list of frustrations and you have
to choose between endpointing, latency and discoverability.
