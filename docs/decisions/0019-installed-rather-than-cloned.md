# 0019 — Installed rather than cloned

**Status:** accepted
**Date:** 2026-09-11
**Builds on:** [0015](0015-settings-come-from-the-keyring-not-the-environment.md)

## Context

Running IRA meant cloning the repository, running a setup script, and
`cargo run`. That is the right shape for the person writing her and the wrong
shape for everybody else: it asks for a Rust toolchain, a git client and a
terminal to use a voice assistant.

The obvious answer is an installer, and the obvious answer is not the hard part.
Every path in the program was relative to the working directory — `models/`,
`piper/piper.exe`, `ira.local.db`, `skills/`, `transcript.jsonl` — which is
exactly right in a checkout and false everywhere else. An installer puts the
binary somewhere the user cannot write, and a Start-menu shortcut launches with
a working directory nobody chose. Ship an installer without touching that and
the first run writes its database into `C:\Windows\System32` or fails trying.

Three more things only become visible once she is installed rather than run:

**The models are not in the repository.** They are 85 MB of weights fetched by
`scripts/fetch-models.ps1`, and an installed IRA has no `scripts/` directory.
Every error message naming that script named a file the reader did not have.

**A console window appears.** She is a console program, correctly — `ira set`,
`ira doctor` and `ira fetch` are how she is configured. Double-clicked, that
console is a black rectangle sitting behind the orb for the whole session.

**And a failure is invisible.** A shortcut launch that fails a preflight check
prints its reason into a window that closes at the same instant.

## Decision

**One place answers "where", and a checkout still wins.** `paths.rs` resolves
`IRA_DATA`, then a `Cargo.toml` in the working directory, then the platform's
per-user directory — `%LOCALAPPDATA%\IRA`, `~/.local/share/ira`,
`~/Library/Application Support/IRA`. The middle rule is what keeps `cargo run`
in a checkout behaving exactly as it did: the models you fetched into `./models`
are still the ones she loads, and an installed IRA on the same machine keeps her
own state elsewhere. `IRA_DATA` is first because the tests need it — several
chdir into a temp directory, which has no `Cargo.toml`, and would otherwise
write the settings of the IRA the developer actually uses.

**She downloads her own models, on the first start.** `ira fetch` is the same
pinned URL list the scripts use, in Rust, and it runs itself when the files are
absent rather than failing a check and stopping. The scripts stay: they are what
CI runs and what a checkout uses. Every "missing" message now names `ira fetch`,
which is on the machine by definition, being the program printing it.

Bundling the models into the installer was the alternative. It was rejected for
two reasons: it triples the download for everyone including the people who
already have them, and it turns IRA into a redistributor of four upstream
licenses rather than a program that fetches four files.

**Offline speech-to-text stays opt-in.** `ira fetch --whisper`, because it is
several times the size of everything else and she talks to Groq by default.

**The console closes once she is running, not before.** A console with only one
process attached was given to her by a shortcut and can be closed; one shared
with a shell is somebody's terminal. `GetConsoleProcessList` distinguishes them.
It is freed *after* startup succeeds, so a first run's 85 MB download is
watchable, and standard error becomes `ira.log` in the data directory first,
because a program with no console has nowhere to say anything. If startup fails
before that point, she prints the reason and waits for Enter — a window that is
about to close should not take the reason with it.

The usual approach — declare `windows_subsystem = "windows"` and call
`AttachConsole` — was rejected. It hides the window at the cost of every
subcommand in this program printing after the shell has already returned to its
prompt.

**Per-user installs, on every platform that has one.** The MSI installs into
`%LOCALAPPDATA%\Programs\IRA` with no administrator rights and no UAC prompt.
Uninstalling removes the program and deliberately leaves the data directory:
an upgrade uninstalls the old version first, so cleaning up there would delete
someone's transcript, skills and models every time they updated.

**Nothing is signed.** SmartScreen will warn on Windows and Gatekeeper will
refuse on macOS until certificates exist. The release notes say so in plain
words rather than leaving people to discover it. Signing is a `signtool` step
away and needs no change to the packaging.

## Consequences

Someone can download an installer, run it, and talk to IRA without knowing what
Rust is. The first start costs 85 MB and says so.

**macOS ships unverified.** The orb and the settings window are Windows-only
([0013](0013-the-orb-is-an-overlay-on-the-same-stream.md)), so a Mac gets IRA
with no interface beyond the page she serves — and nobody has a Mac to confirm
even that much starts. CI proves it compiles. That is the whole of the claim,
and the release notes make it in those words. See
[README: what has not been verified](../../README.md#what-has-not-been-verified).

**Two ways to download the models now exist.** `ira fetch` and the shell
scripts, with the same pinned URLs in both. They can drift. The scripts stay
because CI's smoke job proves the README's instructions are true, which is a
claim about the scripts specifically — but a URL changed in one and not the
other is a real failure mode with nothing watching for it.

**The data directory outlives the program.** Uninstalling leaves it, which is
right for an upgrade and surprising for someone who wanted IRA gone. The README
says where it is.
