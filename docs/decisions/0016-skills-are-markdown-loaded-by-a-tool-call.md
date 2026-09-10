# 0016 — Skills are Markdown files, loaded by a tool call

**Status:** accepted
**Date:** 2026-09-10

## Context

Everything IRA can be taught so far is a *capability*: an MCP server, or
Wingman. Both are processes that run. There was no way to give her a page of
instructions — how this user writes a standup, which of two names means which
person, what "the usual" means on a Thursday — without editing `llm.rs` and
rebuilding.

That is a different kind of extension from a tool, and the roadmap
[already rejected](../ROADMAP.md) a second *executable* extension mechanism:
Echo shipped a dynamic-library plugin system whose permissions were advisory and
unenforced, and MCP gives out-of-process isolation without inventing another.
None of that argument applies to text.

## Decision

**A skill is one Markdown file in `skills/`.** The filename is the name — not a
field, so it cannot disagree with the file it is in. Front matter holds one
`description`. Everything else is the body.

**The catalogue is always in the prompt; the bodies are a tool call.** The model
is shown `name: description` for every skill in the `skill` tool's own
description, and calls `skill(name)` to get one body.

The alternative — concatenating every skill into the system prompt — is simpler
by one tool and wrong on cost. The system prompt is paid for on every round of
every turn, and a tool-calling turn has two rounds. Ten skills of a page each
would put more in front of the model per turn than most turns contain, in a loop
whose whole design budget is measured in hundreds of milliseconds. This is
exactly the trade `only` already makes for MCP tools in `ira.toml`
([0002](0002-tools-behind-a-trait-mcp-via-one-adapter.md)), and it is made the
same way for the same reason.

**Bodies are loaded into memory at start-up, and the tool does no file IO.** It
resolves a name against the loaded list. There is no path in its arguments, and
`name` is an `enum` in its schema, so a model asking for `../../.ssh/id_rsa`
finds no such skill rather than finding a file. A tool that took a path would
have to defend against that; one that cannot express a path does not.

**No front matter parser.** The one key that matters is `description`, and a
YAML dependency that can handle anchors and block scalars to read one string is
doing far more than is wanted. A missing description falls back to the body's
first non-blank line, because refusing to load a skill over a field the user did
not know to write hides it for no benefit.

**Not a `Tool` implementation in `tool.rs`.** It is registered exactly like one
and the registry cannot tell the difference — which is the point of the trait —
but loading and parsing live in `skills.rs` beside the thing they serve.

**Registered only when at least one skill loaded.** A `skill` tool with an empty
list is a tool the model can see and cannot use, which is the same rule Wingman
follows ([0012](0012-wingman-is-a-built-in-not-an-mcp-shim.md)).

## Consequences

Adding a skill is dropping a file in a directory. No config file, no code, no
schema — which is the whole point, since the audience for this is the person
using IRA rather than the person building her.

A new or edited skill needs a restart, unlike provider settings, which are
per-request ([0015](0015-settings-come-from-the-keyring-not-the-environment.md)).
Start-up is a model load, so this is the same minutes-long loop 0014 complained
about — acceptable here only because writing a skill is a rarer act than fixing
a mistyped key.

Whether a skill is *used* is the model's judgement, from one line of
description. A skill that never fires usually has a description that says what
it contains rather than when to reach for it. There is nothing in the loop that
can detect this, and no test can either.

The caps are guesses that could bite: 200 bytes of description and 16 KB of
body. Both truncate with a note rather than silently, because instructions cut
mid-sentence otherwise read as instructions the model chose to ignore.

## What would change this

Skills wanting to bundle files — a template, a checklist, an example — which is
a directory per skill rather than a file, and a way to reference what is in it.
Worth doing when someone actually writes one that needs it; not before.

Enough skills that the catalogue itself is a cost. At that point the description
list becomes a search rather than a menu, which is a real feature and not this
one.

A skill wanting to run something. That is an MCP server, and the answer stays no
for the reasons in the roadmap.
