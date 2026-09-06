# 0001 — The audio path stays in one process

**Status:** accepted
**Date:** 2026-09-06

## Context

Every pressure toward extensibility pushes components out of the process: tools
as subprocesses, transcription as a service, the UI as a separate app. Applied
uniformly, that produces a pipeline of programs — which is how the obvious
"combine Echo and Wingman with a script" design arrives (see
[0008](0008-rejected-assembling-ira-from-echo-and-wingman.md)).

Barge-in pushes the other way. It works today because one process holds the
microphone and the speaker in the same instant and shares one
`CancellationToken` across transcription, generation and playback. Interrupting
is a single `cancel()` that drops the HTTP stream mid-flight and clears the
audio queue in the same frame — microseconds, not a round trip.

## Decision

Capture, wake word, VAD and playback live in one process with shared
cancellation. `audio.rs`, `wake.rs`, `vad.rs` and `tts.rs` are closed to
extension: no plugin, tool or integration may sit in the audio path.

Tools go out-of-process. Audio never does.

## Consequences

Interruption stays sub-frame, which is the product's core claim.

The cost is that changing anything in the audio path means editing core files
deliberately, with latency measured before and after — there is no seam to hide
behind. P6 (AEC) is the only planned phase that touches them.

"Closed" means no extension point, not immutable. `main.rs` is closed for the
same reason: a state machine with pluggable transitions is one nobody can reason
about, and this one has already shipped a race.

## What would change this

A measurement showing cross-process cancellation can meet NFR-2 (< 400 ms from
user speech to silence) on the target hardware. Shared-memory audio with an
atomic kill flag might do it; nothing cheaper will.
