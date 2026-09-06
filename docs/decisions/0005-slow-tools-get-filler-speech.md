# 0005 — Slow tools get filler speech

**Status:** accepted
**Date:** 2026-09-06

## Context

A tool call inserts seconds between the user's question and IRA's answer. The
loop has no way to express "working on it" — today it either speaks or is
silent.

Multi-second dead air reads as a crash. The user repeats themselves, which
triggers barge-in, which kills the turn that was about to succeed. The failure
compounds.

Three alternatives were considered: an earcon while working (cheap, reuses the
chirp machinery, less natural); silence (simplest, and the failure above);
rejecting tools slow enough to need it (keeps the loop tight but rules out
Wingman entirely).

## Decision

Tools declare a latency class in `ToolSpec`: `Fast` (< 300 ms, no filler),
`Slow` (< 15 s, holding phrase first), `Background` (unbounded, returns
`Started(JobId)`).

`Slow` speaks a fixed holding phrase before the result. Fixed, not
model-generated — a filler that needs a model round trip defeats its own purpose.

## Consequences

Dead air stops reading as a crash, using the streaming TTS already built.

A spoken sentence cannot be recalled, so the model must either speak or call a
tool — never narrate half a sentence and then decide to call one. This is
enforced in the system prompt, and by dropping text deltas that arrive in the
same content block as a tool call.

## What would change this

Tool latency dropping below perception across the board, which would make the
`Fast`/`Slow` distinction unnecessary. `Background` would remain.
