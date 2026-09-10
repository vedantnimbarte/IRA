# 0018 — Integrating other tools, in both directions

**Status:** accepted
**Date:** 2026-09-10
**Builds on:** [0017](0017-servers-and-skills-are-configured-in-the-window.md)

## Context

[0017](0017-servers-and-skills-are-configured-in-the-window.md) made servers and
skills editable in the window. Using it revealed that "add any MCP server" was
really "add any MCP server that needs no credential" — a minority of the
interesting ones. Three gaps, all of them the same gap seen from different
sides.

**A server could not be given anything.** `mcp.rs` spawned a child with no
`.env()` at all, so the only way to hand a server its `GITHUB_TOKEN` was to set
it in the shell that launched IRA — exactly the pattern
[0015](0015-settings-come-from-the-keyring-not-the-environment.md) removed for
IRA's own keys, reintroduced for everyone else's.

**A hosted server could not sign in.** Static headers only, which covers a
company gateway and none of the services that actually speak OAuth.

**And nothing could reach IRA.** `POST /talk` opens the microphone; that was the
whole inbound surface. She could call other tools and none of them could call
her.

## Decision

**A server is given named variables; the values are in the keyring.** `mcp_env`
holds `(server, name)` and nothing else — a value lives at
`IRA/env/<server>/<NAME>`, the same store as IRA's own keys and for the same
reason. A variable named but never given a value is passed *not at all* rather
than as an empty string, which many servers read as "configured" and then fail
obscurely.

The child still inherits the rest of the environment, because `PATH` is how a
command is found. What it no longer inherits is anything of IRA's that matters,
since 0015 took her keys out of the environment entirely.

**Removing a server wipes its secrets first.** `db::server_delete` drops the
rows that say *which* variables a server had, so tidying the keyring afterwards
would have nothing to look them up by. `mcp::forget` does both in the order that
works, and every keyring failure inside it is logged and stepped over: a removal
that stops half way is worse than one that could not tidy up. Without this, a
token for a server nobody can see any more sits in Credential Manager forever —
and a reused server name would silently inherit it.

**Signing in is rmcp's OAuth, with three things it cannot know.** Discovery,
dynamic client registration, PKCE, the exchange and the refresh are all
`AuthorizationManager`. What `oauth.rs` supplies is where to redirect
(`http://127.0.0.1:8180/oauth/callback` — she already serves on loopback, which
is the only reason this works with no public URL), where to keep the tokens (the
keyring, as JSON, rewritten by rmcp on every refresh), and how to hold a
half-finished sign-in across two requests (a map keyed by the CSRF state, which
is removed when used, so a replayed redirect finds nothing).

The callback route cannot be origin-guarded — a redirect from a provider is by
definition cross-site. The `state` is the guard: issued by us, held in memory,
good once.

**`POST /say` and `GET /state`.** `/say` queues words for when IRA next has the
floor — the same queue a finished background job uses, because it is the same
problem: news from outside the turn, which must not interrupt the sentence
someone is in the middle of. `/state` is a poll for anything that would rather
not hold the event stream open.

Both are guarded like `/talk` rather than like `/settings/admin` — a client that
states no origin is allowed — because being scriptable from `curl`, a CI job or
a build hook is the entire point, and neither can run anything.

**"Try it" runs a tool from the window, bypassing the confirmation gate.**
Deliberately: the gate exists because *the model* chose the tool. Here a person
pressed a button with the tool's name on it in their own settings window, which
is the confirmation. Asking out loud for a button they just pressed is theatre.
The result is shown raw — this is a wiring check, and a model's phrasing of a
failure would hide it.

**A background job's report is phrased by the model.** It used to be read as
returned, which is a marked shortcut in 0017's wake: tools return JSON and diff
stats, and reading those aloud sounds like a machine. One non-streaming round,
spawned rather than awaited so the loop never stops for it, falling back to the
raw text on any failure — hearing it awkwardly beats not hearing it. The pip
still fires the instant the job lands, because the pip is the news and it must
not wait on a round trip.

**A skill may be a folder.** `skills/standup/SKILL.md` with files beside it,
alongside the flat `skills/standup.md`. This is
[0016](0016-skills-are-markdown-loaded-by-a-tool-call.md)'s own "what would
change this". The files are *listed* to the model and fetched one at a time, for
the same reason the bodies are a tool call and not the system prompt. Writing
still produces the flat form unless the folder already exists, so nobody gets a
directory they did not ask for.

**`ira mcp` and `ira skill`.** The same two stores through the same functions,
for provisioning from a script and for a machine reached over SSH. They do not
connect anything — writing to the database is safe from a second process,
reaching into a running IRA's registry is not — and they do not ask, because a
terminal on this machine already is the authorisation: someone who can run
`ira mcp add` can run the command directly.

## Consequences

`reqwest` moves to 0.13 to match rmcp's. The tree already carried both, so this
removes a duplicate rather than adding one, and no call site changed.

Two path-joins now take a name from the network: a skill's bundled file, and a
server's environment variable. Both are validated where the join happens rather
than at the caller, and both have a test that feeds them `..`.

The window can spawn a browser tab. `window.open` to the provider, in a new tab
rather than this one, because losing the settings page mid-sign-in leaves you at
a provider with no way back.

A test-suite bug surfaced and is fixed: several tests `set_current_dir`, which
is process-wide, and they raced. It presented as a lost database row — a long
way from the cause. They share one lock now.

Nothing here has been verified against a real service. The OAuth flow has never
run against a provider, `POST /say` has never been spoken aloud, and "Try it"
has never called a real MCP server, because this machine has no models and the
loop has never booted.

## What would change this

A server that needs a file rather than a variable — a service-account JSON, a
certificate. That is a third secret shape and the keyring is the wrong store for
it.

Anyone wanting `/say` from another machine. Everything here assumes loopback and
one user; a port reachable from elsewhere needs authentication, not an origin
check.

`/state` growing into a general query API. It is one object for a poll. The
event stream is the real integration point and always was.
