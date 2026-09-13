# 0020 — She runs whisper herself, and local is the default

**Status:** accepted
**Date:** 2026-09-13
**Amends:** [0003](0003-stt-and-model-are-urls.md)

## Context

[0003](0003-stt-and-model-are-urls.md) made local transcription a URL: point
`IRA_STT_URL` at a whisper-server and no audio leaves the machine. It worked,
and almost nobody would have used it. It meant downloading the right build,
starting a server in its own window, keeping that window open, and pasting a URL
into settings — every time the machine restarted.

Echo already solves that part. It starts whisper-server itself, restarts it when
it dies or the model changes, picks a CUDA or CPU build, and falls back when one
will not run. IRA had lifted Echo's pack detection for `ira fetch --whisper`
([0008](0008-rejected-assembling-ira-from-echo-and-wingman.md)) and stopped
there.

## Decision

`IRA_STT_ENGINE` is `local` or `cloud`, and **local is the default** — for fresh
installs and for upgrades alike.

- **Local** runs the whisper-server in `whisper/` with the model
  `IRA_WHISPER_MODEL` names (`tiny.en` on CPU, `small.en` on a CUDA pack when
  unset). IRA starts it at start-up and stops it on exit. On Windows a job
  object also takes it down when IRA is killed. `IRA_STT_URL` still works, now
  as an override meaning "use my server instead".
- **Cloud** is Groq, and falls back to local when local is installed.
- **Local never falls back to cloud.** Choosing local is choosing that audio
  stays here, and a failure path must not quietly break that.
  `stt::route` decides the order, and its test asserts this.

Within local, three levels, each one step slower: the resident server; the CPU
pack after a CUDA failure; `whisper-cli` when no server will run at all.

On Windows, `ira fetch --whisper` (and the first start, and switching to local
in the window) downloads the pack for this machine. It checks each archive
against a pinned SHA-256 before extracting, because what comes out is an
executable IRA runs. Beside a CUDA pack it also fetches the CPU pack into
`whisper/cpu/`. Linux and macOS have no prebuilt binaries: there IRA refuses to
start until whisper.cpp is built, and says how.

Still no provider trait. `route` returns a list of three variants.

## Consequences

A first start is bigger: the tiny.en model (75 MB) and the CPU pack (3.5 MB) on
most machines, and the CUDA 12 pack (443 MB) plus small.en (466 MB) where an
NVIDIA driver supports CUDA 12. No Groq key is needed to start.

Upgraded installs switch to local on their next start whether or not they had a
Groq key, and on a CPU-only machine a turn gets slower. Groq is one setting
away.

A fresh Linux or macOS install no longer runs straight after installing.
That is the cost of local-by-default on platforms upstream does not ship
binaries for, accepted deliberately.

The CUDA 11.8 archive leaves cuBLAS out, so on a machine without the CUDA 11
toolkit its server exits at once with a missing DLL. `ira fetch` used to prefer
it for being 45 MB, which was only safe while running whisper was opt-in. A
driver that supports CUDA 12 now gets the 12.4 pack, which carries cuBLAS
itself. An existing 11.8 install falls back to CPU on its first turn.

## What would change this

Groq becoming meaningfully better at hearing than the models a consumer GPU can
run, by a margin people notice in conversation. Or a streaming local model, which
would change what "the server" is rather than whether IRA runs it.
