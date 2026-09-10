# 0014 — Settings are editable while she runs, and keys are not in a file

**Status:** accepted
**Date:** 2026-09-10

## Context

Every provider setting was an environment variable: `ANTHROPIC_API_KEY`,
`GROQ_API_KEY`, `IRA_LLM_URL`, `IRA_LLM_KEY`, `IRA_LLM_MODEL`, `IRA_STT_URL`.
That is the right default — it is how a key gets into a process without touching
a disk — and it has two costs that show up in use.

A wrong key is discovered by IRA saying she could not hear you, or going silent
three seconds into a sentence. Fixing it means quitting, setting the variable in
the right shell, and starting again — and start-up is a model load, so the loop
of "try a key, hear the failure, try another" is minutes long for a typo.

And there is nowhere to *look*. `doctor` reports which variables are set, but a
voice assistant whose entire configuration is invisible unless you remember the
names is one you configure once and never touch.

## Decision

A gear appears on the orb while the pointer is over it. Pressing it opens a
settings window: keys, URLs and model ids, six fields, saved one at a time.

**Saved values sit over the environment, and are read per request.** A small
override map is consulted before `std::env::var`, by a `settings::get` that
replaces every environment read in `llm.rs` and `stt.rs`. Those reads already
happened per request rather than at start-up, so a key entered now is used by
the next sentence with nothing restarted.

**The environment itself is never written.** `std::env::set_var` would be two
lines and would work for the same reason — but it races with the `getenv` on the
audio, model and orb threads, which is why Rust 2024 made it `unsafe`. This
process has too many threads to take that on to save eighteen lines.

**Keys go to the Windows Credential Manager, not to a file.** They are encrypted
at rest, per user, and cannot be committed by accident. `ira.toml` was the
obvious place and is the wrong one: its MCP blocks are shareable config, the
kind of thing that belongs in a repository, and it is not in `.gitignore`.
Non-secret settings go to `ira.local.toml`, which is.

**A key is never read back.** The settings page is told whether a value is
stored, never what it is, so the window cannot show you a key you have
forgotten — and neither can anything else that can reach the port. Saving is
guarded exactly as `POST /talk` is: a browser claiming to be somewhere else is
refused, a client that says nothing is not.

**Only the six fields can be written.** The save takes a name and looks it up in
a fixed list, so a crafted POST cannot reach through and set an arbitrary
variable.

**The window is shaped as a turn, not as a list.** The six values are exactly
the two stages that leave this machine -- everything before transcription
already runs here -- so they are grouped as hearing and answering, in the order
they happen, rather than as six equal rows. And each field says where its value
came from: *saved* and *set in the shell you started her from* look identical
otherwise, and they are answers to different questions, notably "why is she
using that model when I never chose it". An empty field says what IRA does
instead of it -- "Talking to Anthropic" -- rather than only that it is empty.

**The settings window is a webview**, having just spent
[0013](0013-the-orb-is-an-overlay-on-the-same-stream.md) proving one cannot be
transparent. Nothing about a settings window wants to be: the orb stays drawn
because the orb needs alpha, and a form does not. The alternative was drawing
text fields with a rasteriser, which is building a GUI toolkit to avoid a
dependency. It brings no windowing crate with it — `WebViewBuilder::build` takes
anything implementing `HasWindowHandle`, and the orb thread already owns a
window class and a message loop, so the settings window is another
`CreateWindowExW` on the same loop.

## Consequences

`wry` comes back, for one window that does not need to be transparent, and
`raw-window-handle` with it. No windowing crate, and the orb is untouched.

The page is served from the screen's own server, so it is the same two routes
and one content type `ui.rs` already was, and it works in a browser tab at
`/settings` with no window at all — which is how it is looked at when something
is wrong with the window.

Settings are per machine and outlive a restart, which they did not before. The
environment still works and still loses to a saved value; clearing a field
clears it rather than falling back, so there is a way back to the default
provider from the window.

Secrets are Windows-only. Elsewhere `settings::secret` refuses to write and the
keys stay environment variables, which is exactly where they were.

The failure this design accepts: a saved setting is invisible to `doctor`'s
advice about which variables to set, because `doctor` reports on the
environment. Someone who saved a key in the window and then reads the doctor
output will be told to set a variable they no longer need.

## What would change this

Wanting the timing constants in the window. They are `const` in `main.rs`
deliberately — changing `ENDPOINT_MS` is a decision about how IRA behaves, not a
preference — and putting them behind a form invites turning them without knowing
what they cost.

Wanting the MCP server list editable. That is `ira.toml`, it is shareable, and
the per-tool confirmation policy is a safety mechanism: a UI that edits the
gate on "does this tool change anything" needs more care than a text box.

Wanting settings on macOS or Linux, which is a credential store this does not
have and a window this does not have.
