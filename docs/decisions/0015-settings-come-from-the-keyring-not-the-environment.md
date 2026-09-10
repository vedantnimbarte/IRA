# 0015 — Settings come from the OS keyring and a database, never the environment

**Status:** accepted
**Date:** 2026-09-10
**Amends:** [0014](0014-settings-are-editable-while-she-runs.md)

## Context

[0014](0014-settings-are-editable-while-she-runs.md) put a settings window over
the environment: a saved value won, and an unsaved one fell through to
`std::env::var`. That kept the old path working and left three problems it had
already noticed and accepted.

**A key on a command line is a key in four places.** Shell history, the process
table, whatever CI log echoed the step that set it, and the environment of every
child IRA spawns — Piper, and every stdio MCP server in `ira.toml`. A server
from `ira.toml` is configuration, not code we wrote, and it inherits
`ANTHROPIC_API_KEY` for no reason at all.

**Two sources for one value is two answers to "why is she using that model".**
The window grew a line under every field saying which of the two it was reading,
which is a UI paying rent on a design decision.

**Off Windows there was no store**, so `settings::secret` refused to write and
keys stayed environment variables — meaning the window did not work at all on
the two platforms where it would have to fall back.

## Decision

**The environment is not read for provider settings.** `settings::get` consults
one map, loaded once at start-up from two stores and written through by the
window and by `ira set`. `IRA_STT_URL`, `IRA_LLM_URL`, `IRA_LLM_MODEL`,
`ANTHROPIC_API_KEY`, `GROQ_API_KEY` and `IRA_LLM_KEY` stop being environment
variables. Everything else in [SPEC.md](../SPEC.md#environment-variables) —
paths, ports, the replay switches — stays exactly as it was. Those are not
secrets and are not things anyone reconfigures from a window.

**Keys go to the OS keyring, via the `keyring` crate.** A dependency, against
this repository's grain, and it earns it: it replaces 80 lines of `unsafe` Win32
FFI that worked on one platform with one that works on three — Credential
Manager, Keychain, Secret Service. The alternative was writing the other two
bindings ourselves, or shelling out to `security` and `secret-tool`, which is a
worse version of the same crate with the failure modes discovered later.

**Everything else goes to SQLite, in `ira.local.db`.** One table, `settings
(name, value)`, six rows at most. It replaces a hand-rolled TOML file and the
escaping function it needed — a URL with a quote in it wrote a file that would
not parse, and the failure landed at the *next* start-up, so it read as settings
being forgotten rather than as a bad value. A bound parameter cannot do that.

**`ira set <NAME> [VALUE]` is the way in.** Without the environment there is a
deadlock at first run: the fatal start-up check for a missing key fires before
the orb exists, so there is no window to type one into. It shares one code path
with the window's save, prints the name and never the value, and clears when
given no value.

**`settings::load()` moves above the fatal start-up checks**, because those
checks are checks on these values. It used to run after them and got away with
it only because `doctor` read the environment directly. `doctor` and `metrics`
now read `settings::get` too, which also fixes 0014's accepted failure: `doctor`
no longer tells you to set a variable for a key you already saved.

## Consequences

Two new dependencies, `keyring` and `rusqlite` (bundled, so no system SQLite).
`rusqlite` brings a C build of SQLite, which is a compile-time cost on a cold
build and nothing at runtime.

Existing installs lose their settings once: `ira.local.toml` is not read, and
environment variables are ignored. `ira set` re-enters six values, or the window
does. No migration code, for six strings someone typed once.

The settings window loses its "coming from the environment" line, and the
`Source` enum with it, because there is now one source per field.

Off Windows there is a keyring for the first time — but on a headless Linux box
there is often no Secret Service running, and there the keyring fails where the
environment used to work. `Entry::new` returning `NoDefaultStore` is reported as
"no OS keyring on this machine", naming gnome-keyring, rather than as the
crate's own wording.

CI stops passing provider settings as environment variables and writes
`ira.local.db` instead. The smoke job needs no keys — it points at a local
whisper-server and a stub model — so it never touches the keyring, which a CI
runner does not have.

## What would change this

A deployment where the process genuinely has no user session — a container, a
service account — and therefore no keyring. That wants a third store read from a
file path given on the command line, not the environment variable this removed.

Someone needing more than six settings, or settings with structure. The database
is there and a second table is cheap; the window is what would not scale.
