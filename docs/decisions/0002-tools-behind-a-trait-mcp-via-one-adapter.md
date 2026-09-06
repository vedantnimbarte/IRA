# 0002 — Tools sit behind a Rust trait, with MCP through one adapter

**Status:** accepted
**Date:** 2026-09-06

## Context

Memory, calendar, home automation, Wingman and Echo are all wanted as
capabilities. They have incompatible shapes and wildly different latencies: a
memory lookup is sub-millisecond and local; a Wingman coding turn takes minutes
and runs a build. Hardwiring each one is what this decision exists to avoid.

Two pure options were considered.

**MCP for everything.** Maximum uniformity, one code path, a large existing
ecosystem. But a local recall lookup would pay process spawn and IPC overhead
inside a latency-critical loop, to gain consistency no user experiences.

**A Rust trait only.** Fastest, no schemas, no IPC. But every new capability
becomes a code change and a rebuild — precisely the hardwiring being removed.

## Decision

One internal `Tool` trait. Built-ins that must be instant implement it directly.
MCP servers reach it through a single adapter that also implements it. The
registry cannot tell them apart, and neither can the model.

Type names (`ToolSpec`, `ToolOutcome`, `ToolCtx`) deliberately match Wingman's,
which already has a working rmcp-based MCP client with stdio and
Streamable-HTTP transports.

## Consequences

New capabilities arrive as configuration rather than code. Lifting Wingman's MCP
client becomes mechanical rather than a translation layer.

`ToolOutcome` must carry three shapes, not one — `Speak`, `Silent` and
`Started(JobId)` — because a contract of "function returning an answer" would
fit memory and calendar but not Wingman, and discovering that after the registry
was built would mean rewriting it.

Dynamic-library plugins were rejected outright. Echo ships that mechanism and
its own `PLUGINS.md` states the permission list is advisory and unenforced: a
loaded plugin can read the microphone, the transcripts and decrypted keys. MCP
provides real process isolation without inventing a second mechanism.

## What would change this

A built-in tool needing to affect audio directly, which no current design does.
That would reopen [0001](0001-audio-path-stays-in-one-process.md) rather than
this decision.
