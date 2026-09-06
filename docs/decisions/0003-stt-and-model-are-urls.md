# 0003 — STT and the model are URLs, not code paths

**Status:** accepted
**Date:** 2026-09-06

## Context

Both backends were hardwired: transcription posted to Groq, generation posted to
Anthropic. Neither worked offline, and the Anthropic path could not be pointed at
a gateway even though the owner routes through OpenRouter.

Investigation showed both alternatives already speak wire formats IRA emits.
whisper.cpp's `whisper-server` takes the same multipart request as Groq — same
`file` part, same `{"text": ...}` response — and ignores the `model` field IRA
sends. OpenRouter, LM Studio, Ollama, vLLM and llama.cpp all speak the OpenAI
chat-completions format, which differs from Anthropic in exactly three places:
where the system prompt goes, the auth header, and where text sits in each SSE
frame.

## Decision

`IRA_STT_URL` selects any whisper.cpp-compatible server; unset means Groq.
`IRA_LLM_URL` selects any OpenAI-compatible endpoint, with `IRA_LLM_KEY`
(optional, since local servers need none) and `IRA_LLM_MODEL`; unset means
Anthropic.

No provider trait. The branch is a URL and one function, `delta_text`.

## Consequences

Implemented and shipped. Offline STT and OpenRouter both work with no code
change. A local model becomes a config line if VRAM ever allows it.

`IRA_LLM_MODEL` is effectively required on the OpenAI path because gateways name
models differently — OpenRouter wants `anthropic/claude-sonnet-4.5`, not
`claude-sonnet-5`. Getting it wrong produces a 404, which is the most likely
misconfiguration.

Reading one format's frames with the other's rules yields no text and no
exception, so the failure mode is IRA going mute rather than erroring. That is
why `reads_text_from_either_wire_format` asserts each format refuses the other's
frames.

## What would change this

A provider whose streaming format matches neither — Gemini, for instance. That
would justify a third branch, not an abstraction, until there are four.
