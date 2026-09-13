# 0021 — Kokoro is the voice, and Piper is what she falls back to

**Status:** accepted
**Date:** 2026-09-13

## Context

IRA's voice was Piper's `en_US-amy-medium`, and it sounded like a machine
reading. Two things made it so. The voice: Piper's VITS models are fast and
intelligible, and flat. And the shape: every sentence is synthesised alone, so
intonation resets at each full stop.

The candidates were a better Piper voice with its settings tuned (small change,
still recognisably Piper), Kokoro-82M on this machine, or a cloud voice
(ElevenLabs, OpenAI) — most natural of all, but billed per character, dependent
on the network, and slower to start.

Kokoro is an 82M-parameter StyleTTS 2 model under Apache-2.0, and among open
models it is the one people cannot easily tell from a person. It takes IPA
phonemes, not text.

## Decision

`IRA_TTS_ENGINE` is `kokoro` (the default) or `piper`, and `IRA_KOKORO_VOICE`
picks one of eight voices, `af_heart` when unset. Both are lists in the settings
window, and both are heard from the next sentence with nothing restarted.
Picking a voice says a line in it.

**It runs in-process on the `ort` IRA already has.** No new runtime, no new
process. The model, its vocabulary and the voices come from a pinned Hugging
Face commit of `onnx-community/Kokoro-82M-v1.0-ONNX`, about 330 MB, fetched on
the first start while Kokoro is chosen, or by `ira fetch --kokoro`.

**The phonemes come from Piper's espeak-ng.** Piper's download already carries
espeak-ng and its data for exactly this job, so `kokoro.rs` loads that library
rather than fetching another, and applies the same clean-up to its IPA that
kokoro.js does.

**On the CPU.** The full-precision model runs at about 0.4× real time on a
6-core Ryzen 5 5600GT, so every sentence is ready before the one ahead of it has
finished playing. The quantized model was slower on the same CPU, and DirectML
rejects the model's ConvTranspose upsampling outright.

**Piper never goes away.** It stays running underneath, and Kokoro's thread
hands it any sentence it cannot say — every one, if Kokoro will not load. A
voice that fails falls back to a voice, never to silence, and so Kokoro missing
is a warning in `ira doctor` rather than a refusal to start.

## Consequences

She sounds like a person. She also starts speaking later: 490 ms from the first
model token to the first sound, against 186 ms for Piper on the same turn. That
is the cost of a model a hundred times Piper's size, paid once per reply —
sentences after the first are ready before they are needed.

A first start downloads 330 MB more, and Kokoro adds about 1.4 s to start-up
while it loads.

Sentences queue while Kokoro loads, so a voice chosen a moment ago is the one
that says the next line rather than Piper standing in for it.

Barge-in bumps a generation counter. A sentence Kokoro is still synthesising
when interrupted cannot be stopped mid-inference, but its audio is thrown away
rather than played.

## What would change this

A cloud voice that starts speaking faster than Kokoro does locally, at a price
worth paying per sentence. Or a GPU path for Kokoro — CUDA through ONNX Runtime
needs cuDNN, which nothing IRA installs provides.
