# 0012 — Wingman is a built-in, not an MCP shim

**Status:** accepted
**Date:** 2026-09-07

## Context

[0002](0002-tools-behind-a-trait-mcp-via-one-adapter.md) says capabilities
arrive over MCP as configuration rather than as code, and everything since has
honoured that: kortex-memory is four lines of `ira.toml`.

Wingman does not fit. It is an MCP *client* — it consumes MCP servers, it is
not one — so there is no `[[mcp.server]]` entry that reaches it. Something has
to speak its HTTP API.

Two places that something could live:

1. A separate `wingman-mcp` shim: a small server that translates MCP tool calls
   into `wingman serve` HTTP calls. IRA stays pure; Wingman becomes config.
2. A built-in `src/wingman.rs` implementing the same `Tool` trait as the MCP
   adapter.

## Decision

A built-in.

The shim is a whole new project — its own repository, build, release and
version skew — existing to serve exactly one consumer, and it does not remove
the coupling it appears to remove: somebody still writes and maintains the code
that knows Wingman's routes. It only moves that code somewhere with more
overhead around it.

Registration is conditional on `IRA_WINGMAN_URL` naming a daemon that answers
`/v1/health`. Without one, the tool is never added, so a model that cannot use
Wingman is never told about it.

## Consequences

`src/wingman.rs` is 200 lines and IRA now knows three of Wingman's routes. If
Wingman changes them, IRA breaks and must be edited — which is the honest cost,
and is the same cost the shim would have had.

The trait boundary from [0002](0002-tools-behind-a-trait-mcp-via-one-adapter.md)
still holds where it matters: the registry, the confirmation gate and the model
cannot tell this tool from an MCP one. Only its constructor is special.

This confirms [0007](0007-wingman-over-http-not-as-a-library.md), which was
provisional pending P8. HTTP over a crate dependency was the right call: the
process boundary is what lets a coding turn outlive the conversation that asked
for it.

If a second consumer ever wants the same bridge, the shim becomes worth
building and this file is the argument for reversing.
