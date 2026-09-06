# 0009 — The screen is a page IRA serves, not a window it owns

**Status:** accepted
**Date:** 2026-09-06

## Context

IRA needs somewhere to put what it cannot say. Spoken answers are capped at two
sentences by [PRD.md](../PRD.md) principle 2, and since P4 MCP tools return
lists, tables and code that have nowhere to go at all. The system prompt had
also been telling the model to claim it had "put the detail on screen" when
there was no screen, which P1 removed as a lie.

Three surfaces were considered.

**Tauri**, the in-house precedent from Echo. It is a second build pipeline —
node, npm, a Rust webview tree — and an IPC channel, to display text.

**A native GUI crate** such as egui or iced. A large dependency, and a GUI event
loop that wants the main thread, next to an audio loop that must not be blocked.

**A page served over loopback**, read in a browser.

## Decision

IRA serves one page and one server-sent-event stream on `127.0.0.1:8180`,
hand-rolled over `tokio::net`. `IRA_UI=off` disables it; `IRA_UI=<port>` moves
it.

The system prompt's screen clause is included **only when a page is actually
connected**. IRA counts its watchers and does not promise a screen nobody is
looking at.

## Consequences

No new dependency and no second build. Scrolling, selection, copy, zoom, text
sizing and a light/dark theme come from the browser. It is cross-platform
without effort, which P9 wants anyway. Two routes and one content type is less
code than configuring a web framework to serve them.

"Takes no focus" and "does not interrupt the loop when closed" are properties of
a browser tab rather than things to implement: a closed page is a dropped
socket. A page that falls behind misses events rather than slowing the loop —
the broadcast channel drops for a lagging reader instead of applying
backpressure.

The cost is that it is a tab you leave open rather than an overlay that appears
when IRA speaks. That is a real loss of immediacy.

Serving a live transcript of everything said in the room means binding a socket,
so it binds loopback only and never an external interface.

## What would change this

Wanting an overlay — something that appears over other windows as IRA answers,
rather than being visited. That is a real product argument, and it does not
invalidate this: an overlay consumes exactly the same event stream. The
serialised `Event` type is the contract, and a different front end is a
different consumer of it, not a rewrite of anything behind it.
