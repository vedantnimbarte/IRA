# 0007 — Wingman over HTTP, not as a library dependency

**Status:** proposed
**Date:** 2026-09-06

## Context

The README annotates `llm.rs` as becoming `ira-brain` with "Wingman as a
library". Wingman is a 16-crate workspace built for a coding agent: LSP, RAG,
tree-sitter, browser control, TUI, skills, a verification gate. IRA needs one
call site.

Wingman also already ships a headless surface: `wingman serve` with an HTTP API,
`--print` for a one-shot turn, and `--json` for newline-delimited events.

## Decision

Reach Wingman through `wingman serve`'s HTTP API, adapted as one tool with
`latency = "background"`. Do not take a crate dependency.

Provisional — confirm at P8.

## Consequences

IRA's dependency tree stays small and Wingman upgrades independently. The
`Started(JobId)` shape from [0006](0006-background-jobs-return-an-id.md) maps
cleanly onto a detached `serve` run.

The cost is a process boundary and an HTTP hop on a call that already takes
minutes, which is not material.

Wingman stays one tool among many. It is never IRA's brain: its output is a
coding transcript — tool calls, diffs, file paths, verification output — and that
is not speakable. Something would have to decide which fraction to read aloud,
and nothing does.

## What would change this

Needing Wingman's provider abstraction inside IRA's own turn loop rather than its
agent loop as a tool. That would be a dependency on `wingman-providers` alone,
not the workspace.
