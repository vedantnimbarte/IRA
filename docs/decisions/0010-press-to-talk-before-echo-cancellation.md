# 0010 — Press-to-talk ships before echo cancellation

**Status:** accepted
**Date:** 2026-09-07

## Context

Without acoustic echo cancellation the microphone hears the speaker. Only one
state is affected: in `Holding` the VAD picks up IRA's own reply, reaches the
250 ms barge-in threshold, and she interrupts herself. `Idle` and `Listening`
are fine, because IRA is silent in both. Headphones are therefore mandatory, and
the README has called this the biggest gap between the prototype and something
shippable since the beginning.

[ROADMAP.md](../ROADMAP.md) named `webrtc-audio-processing` as the fix. Two
things came out of actually trying it.

**It needs a second toolchain.** The crate builds a bundled C++ library through
meson and ninja. Without them the build fails outright:

```
Error: Failed to execute meson. Do you have it installed?
```

That turns `cargo build` into a multi-toolchain build for everyone who clones
IRA, against a P9 exit criterion of "a clean machine goes from download to
conversation without reading source".

**It cannot be verified from here.** Echo cancellation is a property of a room:
a speaker, an acoustic path, and a microphone. No replayed WAV reproduces it,
because the thing being cancelled is the coupling itself. Every other phase was
verified by replaying audio through the real pipeline; this one cannot be.

Wiring an adaptive filter into the capture path and declaring it done would put
untested DSP into the files [0001](0001-audio-path-stays-in-one-process.md)
closes and requires measuring before and after. Wrong frame sizes, a wrong
sample rate or a mis-estimated delay would quietly degrade capture for every
turn, and nothing here would catch it.

There is also an unsolved architectural precondition. Echo cancellation needs
the far-end signal time-aligned with the near-end capture. `tts.rs` hands
samples to rodio, which owns output timing and does not report it; `audio.rs`
receives capture callbacks from cpal. Aligning them means either tapping the
sink and estimating device latency, or taking playback away from rodio.

## Decision

Ship press-to-talk now. Defer echo cancellation until someone can hold a
conversation on speakers and listen to the result.

`IRA_PTT=1` disarms voice-triggered barge-in. `POST /talk` on the screen's
server takes the floor, or interrupts if IRA is speaking, and a button on the
page does the same.

Independently, TTS now ducks to 35% at the first hint of speech and cuts only
once the interruption is confirmed.

## Consequences

Speakers work today, at the cost of hands-free interruption: interrupting
becomes a button rather than a voice. That is the trade
[0008](0008-rejected-assembling-ira-from-echo-and-wingman.md) already recorded
when rejecting the Echo pipeline, arriving on its own terms.

`POST /talk` is an HTTP endpoint rather than a global hotkey, which keeps IRA
free of a platform-specific input dependency before P9's cross-platform work.
Anyone can bind it to a real hotkey with whatever their OS provides:

```
curl -X POST http://127.0.0.1:8180/talk
```

The talk press is consumed by the next audio frame rather than acting
immediately, so it reuses the state machine instead of duplicating it. At 80 ms
frames that is well inside the interruption budget.

Ducking is a headphone improvement, not a speaker fix. On speakers it lowers
IRA's volume when the microphone hears her, which lowers the echo, which may
stop the VAD firing, which restores the volume. It is disabled under
`IRA_PTT` for that reason.

## What would change this

Someone sitting in front of speakers reporting that press-to-talk is not enough
— that hands-free interruption is the point and a button does not substitute.
Then echo cancellation becomes worth its toolchain, and the first work is the
reference-signal alignment above, not the filter.
