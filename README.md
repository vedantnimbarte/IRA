<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/ira-dark.svg">
    <img src="assets/ira-light.svg" alt="IRA" width="120">
  </picture>
</p>

<h1 align="center">IRA</h1>

<p align="center">A voice assistant you can interrupt.</p>

Wake word → endpointing → speech-to-text → streaming model → streaming speech,
with barge-in. It calls tools, asks before it changes anything, and shows you
what it is doing on a screen.

The bet is that **turn-taking is the feature**. Speech recognition has been good
enough for years; what stays broken is being cut off when you pause to think,
and having to wait out an answer you already know is wrong. A slower model that
yields the floor correctly beats a smarter one that talks over you.

## Run

```powershell
.\scripts\fetch-models.ps1
$env:ANTHROPIC_API_KEY = "sk-ant-..."
$env:GROQ_API_KEY = "gsk_..."
cargo run --release
```

On Linux or macOS, `./scripts/fetch-models.sh` does the same job — though see
[what has not been verified](#what-has-not-been-verified) before trusting it.

Say **"hey Jarvis"**, wait for the chirp, talk. Interrupt her any time. Answer a
follow-up without saying the wake word again.

`cargo run --release -- doctor` checks models, microphone, keys and local
engines before you talk to it. The fatal subset of those checks runs on every
start-up, so a missing key stops the process rather than surfacing as silence
three seconds into your first sentence.

> **Wear headphones, or use press-to-talk.** There is no acoustic echo
> cancellation, so on speakers the microphone hears IRA's own voice and she
> interrupts herself. `IRA_PTT=1` disarms voice barge-in and makes the talk
> control the way to interrupt — speakers work, at the cost of hands-free
> interruption. Why it is not solved properly:
> [decisions/0010](docs/decisions/0010-press-to-talk-before-echo-cancellation.md).

## How a turn works

```
        ┌──────────── Idle ─────────────┐
        │   openWakeWord: 3-stage ONNX  │
        └───────────────┬───────────────┘
                        │ wake word, or POST /talk
        ┌───────────────▼───────────────┐
        │          Listening            │   Silero VAD, 32 ms chunks
        │  pause 200 ms → start STT ────┼──► whisper (Groq, or local)
        │  silence 700 ms → turn is over│    transcription overlaps the wait
        └───────────────┬───────────────┘
                        │ transcript in hand
        ┌───────────────▼───────────────┐
        │           Holding             │   thinking and speaking are one
        │  model streams ──► sentences ─┼──► Piper ──► rodio
        │  barge-in armed the whole time│    duck at a hint, cut when confirmed
        └───────┬───────────────┬───────┘
                │               │ a tool wants to change something
                │               ▼
                │        ┌─────────────┐
                │        │ Confirming  │  spoken yes or no; anything else is no
                │        └─────────────┘
                │ reply done
                ▼
        floor stays open 2 s for a follow-up, then Idle
```

Everything above happens in **one process**. Barge-in works because a single
process holds the microphone and the speaker at the same instant and shares one
cancellation token across transcription, generation and playback — interrupting
is one `cancel()` that drops the HTTP stream mid-flight and clears the audio
queue in the same frame. That is why IRA is not assembled out of separate
programs piped together, and it is the constraint every other decision bends
around: [decisions/0001](docs/decisions/0001-audio-path-stays-in-one-process.md).

| File | Job |
|---|---|
| `audio.rs` | cpal capture, downmix, 16 kHz resample; WAV replay for tests |
| `wake.rs` | openWakeWord: mel → embedding → classifier |
| `vad.rs` | Silero v5, endpointing and barge-in |
| `stt.rs` | Transcription over HTTP, cloud or local |
| `llm.rs` | Streaming generation, sentence splitting, tool loop |
| `tool.rs` | The `Tool` trait, the registry, the confirmation gate |
| `mcp.rs` | MCP servers adapted to that trait |
| `wingman.rs` | Wingman's own HTTP API adapted to that trait |
| `ui.rs` | The screen and its event stream |
| `orb.rs` | The overlay: a drawn globe on a layered window, same stream |
| `main.rs` | The state machine |
| `metrics.rs` · `doctor.rs` · `config.rs` · `transcript.rs` | Timing, preflight, `ira.toml`, the record |

## Tools

A tool is a Rust `impl Tool` or a line of `ira.toml`. The registry cannot tell
them apart and neither can the model.

```toml
[[mcp.server]]
name      = "calendar"
transport = "stdio"          # or "http"
command   = "mcp-calendar"
only      = ["list_events", "create_event"]

# What IRA believes, regardless of what the server says about itself.
# Anything unlisted is assumed to write, and asks first.
[mcp.server.tools]
list_events  = { mutates = false, latency = "fast" }
create_event = { mutates = true,  latency = "slow", confirm = "Add that to your calendar?" }
```

Anything that changes state asks out loud first, and **only an explicit yes runs
it** — silence, ambiguity, and interrupting the question are all refusals. A
server's own description of a tool is never trusted for this, because a tool that
calls itself harmless and is not would otherwise walk straight through the gate.

`latency = "background"` detaches the work: IRA answers immediately, a soft pip
sounds when it finishes, and the words wait until she next has the floor.

### Wingman

[Wingman](https://github.com/vedantnimbarte/wingman) is a terminal coding
agent, and the one capability that is not a line of `ira.toml`: it is an MCP
*client*, not a server, so IRA speaks its HTTP API directly
([decisions/0012](docs/decisions/0012-wingman-is-a-built-in-not-an-mcp-shim.md)).

```powershell
wingman serve                       # defaults to port 8787
$env:IRA_WINGMAN_URL = "http://127.0.0.1:8787"
```

That is the whole setup. `IRA_WINGMAN_TOKEN` is needed only if the daemon
requires one, and `IRA_WINGMAN_PROJECT` only if you want something other than
the first project it lists. Without `IRA_WINGMAN_URL`, or if nothing answers at
it, the tool is never registered and the model is never told about it.

Asking for code by voice is a write, so it asks first, and it is a background
job, so a turn that takes ten minutes does not hold the conversation open.

## The screen

While IRA runs there is a page at <http://127.0.0.1:8180> — live transcript,
tool calls and their results, replies in full, and every turn's timings. It is
a browser tab rather than a window, which is a deliberate trade
([decisions/0009](docs/decisions/0009-the-screen-is-a-served-page.md)).

`POST /talk` takes the floor, or interrupts if IRA is speaking. There is a button
on the page; binding it to a real hotkey is your OS's job, not IRA's:

```
curl -X POST http://127.0.0.1:8180/talk
```

A browser may press it only from IRA's own page: this route opens the
microphone, and a cross-origin post needs no reply to have its effect. Clients
that send no `Origin` or `Sec-Fetch-Site`, curl included, are unaffected.

IRA only tells the model it has a screen while a page is actually open.
`IRA_UI=off` disables it.

### The orb

A tab does not tell you IRA is running while you are working in something else.
So there is also a small always-on-top globe in the bottom-left corner, over
whatever you are doing:

| | |
|---|---|
| pale blue and lilac, barely moving | idle — running, waiting for the wake word |
| cyan and aqua, quickening | listening |
| violet and blue, folding over | thinking |
| cyan and magenta, moving fast | speaking — interrupt her |
| warm pink and amber, almost still | asking whether to do something |
| grey, nothing moving | IRA is not running, or has stopped |

Click it to take the floor, click again to interrupt: it is the talk control,
which is otherwise a button on a page you would have to go and find. The window
hit-tests by alpha, so a click that misses the orb goes to whatever is behind it
rather than to IRA.

Hover it and a gear appears; pressing that opens a settings window.

### Settings

Keys, URLs and model ids, in a window, saved one at a time and used by the next
thing IRA says — nothing restarts:

| | |
|---|---|
| `ANTHROPIC_API_KEY` · `GROQ_API_KEY` · `IRA_LLM_KEY` | Windows Credential Manager |
| `IRA_LLM_URL` · `IRA_LLM_MODEL` · `IRA_STT_URL` | `ira.local.toml`, gitignored |

Saved values sit **over** the environment, which still works and still loses to
them; clearing a field clears it rather than falling back, so there is a way
back to the default provider from the window. Each field says which of the two
it is reading, because "saved" and "set in the shell you started her from" look
identical otherwise, and an empty one says what IRA does instead of it.

Keys never go in a file, and are never read back out — the window is told
whether one is stored, not what it is, so it cannot show you a key you have
forgotten and neither can anything else that can reach the port. Saving is
guarded the way `POST /talk` is. Only those six fields can be written.

It is a webview, having just established that one cannot be transparent: the orb
stays drawn because the orb needs alpha, and a form does not
([decisions/0014](docs/decisions/0014-settings-are-editable-while-she-runs.md)).
The same page is at <http://127.0.0.1:8180/settings> in a tab, which is how to
look at it if the window will not open.

A pearl sphere with iridescent light moving inside it: colour drifting under
the surface, pale ribbons flowing across and folding over each other. There is
no text and no meter, so state is carried by which colours are in it and how
fast they move — which is the whole vocabulary this kind of orb has.

It reads the same events the page reads, straight off the broadcast. It is
**drawn** rather than rendered: a webview window cannot be made transparent on
Windows 11, and five ways of asking were measured before giving up on it, so the
orb is a rasterised bitmap pushed to a layered window. That is also why it has
no HTML and needs no WebView2 —
[decisions/0013](docs/decisions/0013-the-orb-is-an-overlay-on-the-same-stream.md).

The orb deliberately does **not** count as a screen: it shows a colour, not a
transcript, and IRA must not claim to have put anything on it. `IRA_ORB=off`
disables it. Windows only for now, and every way it can fail — no display, a
window that will not open, a bitmap that will not allocate — is a missing light
and never a broken IRA.

## Swapping the brain

Anthropic direct is the default. `IRA_LLM_URL` switches to the OpenAI
chat-completions format, which OpenRouter, LM Studio, Ollama, vLLM and llama.cpp
all speak:

```powershell
$env:IRA_LLM_URL   = "https://openrouter.ai/api/v1/chat/completions"
$env:IRA_LLM_KEY   = "sk-or-..."
$env:IRA_LLM_MODEL = "anthropic/claude-sonnet-4.5"   # the gateway's id, not Anthropic's
```

Whatever the model, keep it fast. Time-to-first-sentence is what you hear — a
reasoning model that deliberates four seconds before its first token feels broken
in a voice loop no matter how good the answer is.

## Running offline

whisper.cpp's `whisper-server` speaks the same multipart API as Groq, so going
offline is a URL rather than a code path:

```powershell
.\scripts\fetch-models.ps1 -Whisper
```

Opt-in, because it is a bigger download than everything else combined. It reads
`nvidia-smi` and picks the build to match — an NVIDIA driver gets the cuBLAS 11.8
pack and `small.en`, anything else the CPU pack and `tiny.en` — then prints the
two lines to run. Numbers and trade-offs: [BASELINE.md](docs/BASELINE.md).

Wake word, endpointing and speech are always local. With `IRA_STT_URL` set, no
audio leaves the machine at all. The model is the one stage that still needs the
network.

## Knobs

Everything worth tuning is a `const` at the top of `main.rs`.

| Const | Default | Symptom if wrong |
|---|---|---|
| `ENDPOINT_MS` | 700 | Too low: cuts you off mid-thought. Too high: sluggish. |
| `SPECULATE_MS` | 200 | Transcription starts here. The saving is the gap to `ENDPOINT_MS`. |
| `BARGE_IN_MS` | 250 | Too low: a cough stops her. Too high: interrupting feels laggy. |
| `BARGE_IN_GRACE_MS` | 300 | Measured from her first sound, not from the start of the turn. |
| `FOLLOW_UP_MS` | 2000 | How long the floor stays open with no wake word. |
| `CONFIRM_TIMEOUT_MS` | 6000 | Silence in answer to a confirmation. Silence is not consent. |
| wake threshold | 0.5 | Too low: fires on the TV. Too high: you repeat yourself. |

Every environment variable is listed in
[SPEC.md](docs/SPEC.md#environment-variables). The ones you are most likely to
want: `IRA_STT_URL`, `IRA_LLM_URL`, `IRA_PTT`, `IRA_UI`, `IRA_CONFIG`,
`IRA_TRANSCRIPT`, `IRA_WINGMAN_URL`.

## Docs

[docs/INDEX.md](docs/INDEX.md) is the map. In reading order:
[PRD](docs/PRD.md) (what and why) ·
[ROADMAP](docs/ROADMAP.md) (ten phases, and where they got to) ·
[ARCHITECTURE](docs/ARCHITECTURE.md) (how, and why it is shaped this way) ·
[SPEC](docs/SPEC.md) (implementable detail) ·
[BASELINE](docs/BASELINE.md) (measured latency) ·
[TEST-PLAN](docs/TEST-PLAN.md) (how we know it works).

Decisions live individually under [docs/decisions/](docs/decisions/) — including
the rejected ones, each with a section saying what would change it.

## What has not been verified

Kept here rather than buried, because it is the honest shape of the project.

- **A live microphone.** Barge-in, the wake word, ducking, press-to-talk and the
  follow-up window are all acoustic, and all of them have only ever been
  exercised by replaying WAV files through the real pipeline.
- **A real model.** Every measurement so far used a local stub, so
  time-to-first-token has never been observed above zero. The latency work in
  [BASELINE.md](docs/BASELINE.md) optimised the transcription half of a budget
  whose model half is unmeasured.
- **macOS, and any ARM machine.** CI now runs the POSIX setup script and a full
  turn on Ubuntu x86-64 every push, which is how two real bugs in it were
  found. The Darwin and aarch64 branches of that script have still never run.
- **Anything driven by a real model.** Every measurement and every completed
  turn used a stub, which is why `ttft_ms` has never been above zero. Wingman
  is connected to the real `wingman serve` 0.3.0 and a turn runs end to end,
  but behind a stub it answers and stops — so it has never edited a file or
  run its verification gate through IRA. kortex-memory is connected and
  verified against a stub of its sixteen tools, never the real server.

## Tests

`cargo test` — 70 tests, no network, microphone or API key needed.

They aim at failures that are **silent** rather than loud, because those are the
ones that survive a code review:

- Silero reporting no speech, ever. Feeding it 512 samples where it wants 576
  runs without error and never fires, so endpointing quietly stops working — a
  test that only checks "silence reads as silence" passes against a dead VAD.
  `speech_reads_as_speech` plays a real recording, and is the one that would have
  caught it.
- Reading one model's SSE frames with the other's rules: no text, no exception,
  IRA simply goes mute.
- A tool from a server that describes itself as harmless walking through the
  confirmation gate.
- A sentence from an interrupted turn being spoken after the interruption.
- A page in another tab pressing talk. `POST /talk` opens the microphone, and
  loopback does not stop a cross-origin post from any site you have open.
- Markdown reaching Piper, so a stray `**` is read out as "asterisk asterisk".
- A state the orb has no colour for. It keeps the last one it knew and says
  nothing, so the light is simply wrong from then on.
- The orb losing its transparency and becoming a square on top of your work.
  `the_corners_are_clear_in_every_state` rasterises a frame and reads the alpha
  back, which is the whole claim without needing a window.
- The orb going white. A broken blur, a mask that clips everything and a grey
  palette all still paint a perfectly convincing sphere.
- A settings value with a quote in it writing a file that will not parse. The
  failure lands at the *next* start-up, so it looks like settings being
  forgotten rather than like a bad value.
- A saved setting that does not win over the environment, or a cleared one that
  quietly falls back to it.

Tests needing a real service skip with a note rather than fail, so a fresh clone
passes: the ONNX ones when `models/` is empty, the local-STT one unless
`IRA_STT_URL` is set.

Set `IRA_AUDIO_FILE` to replay a WAV instead of opening the microphone — that is
how the loop itself is exercised, and how the latency numbers were measured.
There is still no test for "feels right".

## Wake word

`hey_jarvis` is a pretrained openWakeWord model, used so this runs today. A real
"IRA" model is a Colab training run against the same three-stage chain — only
`hey_jarvis_v0.1.onnx` changes, no code does.

openWakeWord is Apache-2.0 and free commercially. Porcupine has a built-in
"jarvis" keyword and is far less code, but its commercial licensing does not fit.

**Wake latency floor:** openWakeWord needs ~2.2 s of audio in its window before
the classifier can fire at all — 76 mel frames to reach the first embedding, then
16 embeddings at 80 ms each. In use the window is always full, so this only shows
up in the first couple of seconds after start-up.

## Deliberate shortcuts

Each is marked with a `ponytail:` comment where it lives.

| Shortcut | Ceiling | Upgrade when |
|---|---|---|
| No echo cancellation | Headphones or press-to-talk | Someone reports the talk button is not enough |
| Fixed 8-turn history | No real memory | kortex-memory over MCP |
| VAD-only endpointing | Cuts off mid-thought pauses | A semantic turn model earns its false positives |
| Cheap linear resampler | Slight aliasing | Only if word-error-rate measurably suffers |
| Piper respawn on barge-in | ~250 ms before she can speak again | If interruption recovery feels slow — keep a warm spare |
| Background jobs in memory | Lost on restart, and reported as lost | Jobs routinely outlive the process |
| Tool results read as returned | Reads like a machine | Hand them to the model to phrase; it costs a turn nobody waits on |
