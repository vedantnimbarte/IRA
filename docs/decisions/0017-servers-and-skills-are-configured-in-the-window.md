# 0017 — Servers and skills are configured in the window, and stored in the database

**Status:** accepted
**Date:** 2026-09-10
**Amends:** [0014](0014-settings-are-editable-while-she-runs.md),
[0016](0016-skills-are-markdown-loaded-by-a-tool-call.md)

## Context

[0014](0014-settings-are-editable-while-she-runs.md) put six provider values in a
window and listed *"wanting the MCP server list editable"* under what would
change it, specifically because the per-tool confirmation policy is a safety
mechanism and *"a UI that edits the gate on 'does this tool change anything'
needs more care than a text box"*. That day arrived.

The gap it left is real. Everything IRA can actually *do* — every tool, every
skill — was configured by hand-editing files she reads once at start-up, while
the six values that merely decide which model answers had a window and applied
mid-sentence. The wrong half was editable.

## Decision

**Servers, their per-tool policy, and which skills are on live in
`ira.local.db`**, alongside the settings [0015](0015-settings-come-from-the-keyring-not-the-environment.md)
put there. Three more tables, all small. `ira.toml` is imported once on the
first start after this lands, a marker is written, and it is never read again —
one source of truth, and a file that kept being re-read would fight the window
at every restart.

**Both apply without a restart.** `Host`'s tool map moves behind an `RwLock` and
gains `set_server` / `remove_server`, keyed by the server a tool came from.
Reads are the hot path — every round of every turn asks for `specs()` — and
writes happen when a person presses Save, so it is almost never contended.
`call` clones the `Arc` out from under the lock before it awaits anything, which
is also what lets a server be removed while one of its tools is still running:
the connection stays alive until that call finishes.

**A stdio server is spoken aloud before it is stored.** This is the part worth
being careful about. A stdio server is a *command IRA spawns*, so moving it from
a file into a form on `127.0.0.1:8180` turns a filesystem-write into whatever
guards that port. It goes through the same confirmation gate a mutating tool
uses — IRA reads the command back and waits for an explicit spoken yes — and
anything that is not a yes, including a loop that is not listening at all,
leaves nothing stored. Turning one back on asks again.

**And that route is stricter than `POST /talk`.** The existing guard deliberately
allows a client that states no origin, so the documented `curl -X POST /talk`
keeps working: it refuses browsers that say they are elsewhere, not clients that
say nothing. That is a sound trade for "open the microphone" and the wrong one
for "spawn this program", so `POST /settings/admin` additionally requires that
something actually said where the request came from.

**The whole policy is in the window**, not just an allowlist: which tools are
offered, whether each is read-only, and what IRA asks before running it. The
window is where the consequence has to be legible, so each row says what it
*does* — "Asks first — nobody has said whether it changes anything" — rather
than naming the field. **Absent is not false**: a tool nobody has judged still
asks, and the page shows that as its own state rather than as an unticked box.

**Skills are the other way round: the window writes files, the database indexes
them.** A row holds the name, path, summary and whether it is on. Bodies stay as
`.md` on disk, because that is what an editor, a diff and a repository can see,
and a skill that lives only in a database row is one you cannot review. This is
a deliberate exception to "store it in the database": the index is the part that
needed to be queryable, not the prose.

**One route with an `op`, not six.** Every change shares a guard, a body reader
and an answer shape, and every one answers with the whole state, so the page
never has to guess what its change did — particularly for a connect, where the
answer is a list of tools it did not have before.

## Consequences

The settings page can now cause a process to be spawned. That is the point, and
it is why two of the decisions above are about the gate rather than the feature.

`same_origin` grew into `read_headers`, which also returns `Content-Length`. The
body reader used to frame a request by its first closing brace — correct for
`{name, value}` and silently truncating for anything nested, which a server's
`headers` object is. That was a latent bug this change would have tripped over.

`Host::add` takes `&self` now. The registry is shared as an `Arc` the moment the
loop starts and the window adds to it after that, so a `&mut` API could not
survive.

A tool's spec is built when its server connects, so saving a policy reconnects
that server. For a stdio server that means the child is restarted, which is a
visible pause and the reason policy edits are not chatty.

Editing a server does *not* clear its tool rows, and deleting one *does*. Both
matter: the first would silently re-arm the gate on tools someone had vouched
for, and the second would silently re-apply old judgements to a reused name.

A skill scan runs at every start-up and must not write `enabled`, or every
restart would turn every skill back on and the switch would look broken. That is
one `ON CONFLICT` clause and its own test.

The window can now make IRA do more than it can undo. There is no confirmation
on removing a server, and none on deleting a skill beyond the browser's own
`confirm()` — the file is gone. Acceptable because both are cheap to recreate
and neither is destructive outside IRA, but it is a choice, not an oversight.

## What would change this

A second machine. All of this assumes one user at one keyboard on loopback; a
settings page reachable from anywhere else needs authentication, not an origin
check.

Enough servers that reconnecting on every policy edit is annoying. The fix is a
registry that can rebuild one tool's spec without reconnecting, which is a real
change to how `McpTool` is built and not worth it for the handful of servers a
voice assistant actually wants.

Wanting to edit the tuning constants. Still no, for the reason 0014 gave.
