"""Scripted OpenAI chat-completions server that records every request."""

from __future__ import annotations

import json
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

# A scripted reply: {"text": "..."} or {"tool_calls": [{"name": ..., "arguments": {...}}]}.
# Optional keys: "usage" ({"prompt_tokens", "completion_tokens"}) and "stall" (seconds to
# wait before answering, to simulate a hung endpoint).
Reply = dict[str, Any]
Responder = Callable[[int, dict[str, Any]], Reply]


@dataclass
class RecordedRequest:
    path: str
    headers: dict[str, str]
    body: dict[str, Any]


@dataclass
class FakeChatServer:
    responder: Responder
    bind_host: str = "127.0.0.1"
    requests: list[RecordedRequest] = field(default_factory=list)
    _httpd: ThreadingHTTPServer | None = None
    _lock: threading.Lock = field(default_factory=threading.Lock)

    @property
    def base_url(self) -> str:
        assert self._httpd is not None
        return f"http://127.0.0.1:{self._httpd.server_address[1]}/v1"

    @property
    def port(self) -> int:
        assert self._httpd is not None
        return int(self._httpd.server_address[1])

    def script(self, replies: list[Reply], *, repeat_last: bool = True) -> None:
        """Serve `replies` in order; after the end repeat the last one (or say 'done')."""

        def respond(index: int, _body: dict[str, Any]) -> Reply:
            if index < len(replies):
                return replies[index]
            return replies[-1] if repeat_last and replies else {"text": "done"}

        self.responder = respond

    def start(self) -> FakeChatServer:
        server = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args: Any) -> None:  # silence
                pass

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers.get("Content-Length", "0"))
                raw = self.rfile.read(length)
                body = json.loads(raw) if raw else {}
                with server._lock:
                    index = len(server.requests)
                    server.requests.append(
                        RecordedRequest(
                            self.path, {k.lower(): v for k, v in self.headers.items()}, body
                        )
                    )
                reply = server.responder(index, body)
                if reply.get("stall"):
                    time.sleep(reply["stall"])
                if body.get("stream"):
                    payload = _sse(body, reply, index)
                    ctype = "text/event-stream"
                else:
                    payload = json.dumps(_completion(body, reply, index)).encode()
                    ctype = "application/json"
                self.send_response(200)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self._httpd = ThreadingHTTPServer((self.bind_host, 0), Handler)
        threading.Thread(target=self._httpd.serve_forever, daemon=True).start()
        return self

    def stop(self) -> None:
        if self._httpd is not None:
            self._httpd.shutdown()
            self._httpd.server_close()


def _tool_calls(reply: Reply, index: int) -> list[dict[str, Any]]:
    return [
        {
            "id": f"call_{index}_{i}",
            "type": "function",
            "function": {"name": c["name"], "arguments": json.dumps(c.get("arguments", {}))},
        }
        for i, c in enumerate(reply.get("tool_calls", []))
    ]


def _usage(reply: Reply) -> dict[str, int]:
    usage = reply.get("usage", {})
    prompt = usage.get("prompt_tokens", 10)
    completion = usage.get("completion_tokens", 5)
    return {
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
    }


def _completion(body: dict[str, Any], reply: Reply, index: int) -> dict[str, Any]:
    calls = _tool_calls(reply, index)
    message: dict[str, Any] = {"role": "assistant", "content": reply.get("text")}
    if calls:
        message["tool_calls"] = calls
    return {
        "id": f"chatcmpl-{index}",
        "object": "chat.completion",
        "created": 0,
        "model": body.get("model", ""),
        "choices": [
            {
                "index": 0,
                "message": message,
                "finish_reason": "tool_calls" if calls else "stop",
            }
        ],
        "usage": _usage(reply),
    }


def _sse(body: dict[str, Any], reply: Reply, index: int) -> bytes:
    calls = _tool_calls(reply, index)
    base = {
        "id": f"chatcmpl-{index}",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": body.get("model", ""),
    }
    chunks: list[dict[str, Any]] = []
    if reply.get("text"):
        chunks.append(
            {
                **base,
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": reply["text"]}}],
            }
        )
    if calls:
        delta = {
            "role": "assistant",
            "tool_calls": [{"index": i, **c} for i, c in enumerate(calls)],
        }
        chunks.append({**base, "choices": [{"index": 0, "delta": delta}]})
    chunks.append(
        {
            **base,
            "choices": [
                {"index": 0, "delta": {}, "finish_reason": "tool_calls" if calls else "stop"}
            ],
            "usage": _usage(reply),
        }
    )
    out = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"
    return out.encode()
