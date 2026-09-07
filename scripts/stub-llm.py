#!/usr/bin/env python3
"""A model that always says the same thing, so the loop can be run without a key.

IRA talks to an OpenAI-compatible endpoint when `IRA_LLM_URL` is set, and this
is the smallest thing that satisfies that contract: a streamed reply, one word
per frame, then `[DONE]`.

It exists for the smoke job in .github/workflows/ci.yml, which proves that a
machine that has only run `scripts/fetch-models.sh` can hold a conversation.
Nothing about that check should depend on a billable API key, an account, or a
network that reaches Anthropic -- a setup script that works must be provable on
a machine that has neither.

    python3 scripts/stub-llm.py &
    IRA_LLM_URL=http://127.0.0.1:8299/v1/chat/completions IRA_LLM_MODEL=stub cargo run

HTTP/1.0 with no Content-Length: the client reads until the connection closes,
which streams without hand-rolling chunked framing.
"""
import json
import os
import socket
import sys
import threading

REPLY = os.environ.get("REPLY", "The smoke test says hello.")
PORT = int(os.environ.get("PORT", "8299"))


def serve(conn):
    try:
        # Drain the request. Not read: this endpoint has one answer.
        conn.recv(65536)
        conn.sendall(
            b"HTTP/1.0 200 OK\r\n"
            b"Content-Type: text/event-stream\r\n"
            b"Cache-Control: no-cache\r\n"
            b"Connection: close\r\n\r\n"
        )

        def frame(obj):
            conn.sendall(b"data: " + json.dumps(obj).encode() + b"\n\n")

        # The opening role-only delta, exactly as a real endpoint sends it.
        frame({"choices": [{"delta": {"role": "assistant"}, "index": 0}]})
        for word in REPLY.split(" "):
            frame({"choices": [{"delta": {"content": word + " "}, "index": 0}]})
        frame({"choices": [{"delta": {}, "finish_reason": "stop", "index": 0}]})
        conn.sendall(b"data: [DONE]\n\n")
    except Exception as e:
        print("stub-llm: %r" % e, file=sys.stderr, flush=True)
    finally:
        conn.close()


s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", PORT))
s.listen(8)
print("stub-llm on %d" % PORT, file=sys.stderr, flush=True)
while True:
    c, _ = s.accept()
    threading.Thread(target=serve, args=(c,), daemon=True).start()
