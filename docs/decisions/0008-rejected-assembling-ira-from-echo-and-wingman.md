# 0008 — Rejected: assembling IRA from Echo and Wingman

**Status:** rejected
**Date:** 2026-09-06

## Context

Echo and Wingman both exist and work. Echo is a voice keyboard: hotkey, speak,
transcribe, type into the focused app. Wingman is a terminal coding agent. The
obvious proposal is to skip building a third thing — run both as background
daemons, script them together, and let Echo forward transcripts to Wingman with
any TTS engine reading the result.

The instinct is right, and has already paid off elsewhere in this project: the
local STT work lifted Echo's whisper-server approach, its model catalogue and
its GPU pack detection rather than reinventing them.

The pieces are also readier than assumed. Wingman has `serve`, `--print` and
`--json`. Echo has an `output` plugin permission, so forwarding a transcript
instead of typing it is a supported extension point rather than a fork.

## Decision

Rejected as the architecture. Retained as a legitimate same-day demo path if the
goal is to show something working rather than to test the loop.

## Consequences

Three reasons, in descending order of severity.

**It is half-duplex by construction.** Barge-in cannot exist in a process chain:
no shared cancellation, and by the time TTS starts, Echo has finished and let go
of the turn. See [0001](0001-audio-path-stays-in-one-process.md).

**Latency becomes additive instead of overlapped.** IRA speaks sentence one while
the model writes sentence two. A pipeline cannot — each stage must complete to
hand off. Worse, Wingman's verification gate runs builds and tests before ending
a turn, which is its best property as a coding agent and a disaster inside a
voice loop.

**Wingman's output is not an answer.** It is a coding transcript. IRA's system
prompt fights for the opposite: at most two short sentences, no markdown, no
lists, no code blocks. Something must decide which fraction is speakable, and
that layer exists in neither project.

The reuse instinct is satisfied instead by
[0002](0002-tools-behind-a-trait-mcp-via-one-adapter.md) and
[0007](0007-wingman-over-http-not-as-a-library.md): Wingman becomes a tool IRA
calls, not a stage IRA pipes through.

One genuine advantage of the rejected design is worth recording: press-to-talk
sidesteps acoustic echo cancellation and false wakes entirely, because the
microphone is not open while the speaker plays. That is captured as FR-7 rather
than lost.

## What would change this

Nothing about the pipeline shape. If duplex turn-taking turns out not to matter
to users — the product's central bet — the whole premise changes, and this would
be the cheaper architecture.
