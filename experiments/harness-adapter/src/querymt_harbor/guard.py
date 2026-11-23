"""Detect safeagents blocks in a finished rollout (design D6).

safeagents sits between the capture proxy and vLLM and replaces a flagged tool call with a text
notice in the response, before the proxy records it, so training sees exactly what the harness
saw. The notice is appended to the assistant message's `content`:

    [safeagents] blocked call to `<tool>`: <reason>

Source of the transcript (the design's open question): TRL's `HarnessRolloutOutcome` carries
both `trace` (every proxy-recorded request/response of the selected turns) and `completion`
(only the last request's messages plus the final reply). A block early in a run is in the
last request's history only as long as the harness keeps its history, so detection reads the
trace, which holds every post-guard response, and falls back to `completion` only when there is
no trace. Tool results are never scanned: an agent that prints the marker from a file must not
zero its own rollout.
"""

from __future__ import annotations

from collections.abc import Iterable, Mapping, Sequence
from pathlib import Path
from typing import Any

BLOCK_MARKER = "[safeagents] blocked call to `"
"""Start of safeagents' `Blocked.notice()`; a unit test pins it to the installed version."""

METRIC = "guard_blocked_rollouts"


def _text(content: Any) -> str:
    """Flatten message content: a string, or a list of `{"text": ...}` parts."""
    if isinstance(content, str):
        return content
    if isinstance(content, Sequence):
        return "\n".join(
            part["text"] for part in content if isinstance(part, Mapping) and part.get("text")
        )
    return ""


def _response_texts(trace: Iterable[Mapping[str, Any]]) -> Iterable[str]:
    for entry in trace:
        response = entry.get("response") or {}
        for choice in response.get("choices") or []:
            yield _text((choice.get("message") or {}).get("content"))
        for block in response.get("content") or []:  # Anthropic-shaped responses
            if isinstance(block, Mapping):
                yield _text(block.get("text"))


def _completion_texts(completion: Iterable[Mapping[str, Any]]) -> Iterable[str]:
    for message in completion:
        if message.get("role") == "assistant":
            yield _text(message.get("content"))


def count_blocked(trace: Sequence[Mapping[str, Any]] | None, completion: Any = ()) -> int:
    """How many tool calls safeagents blocked in this rollout."""
    texts = _response_texts(trace) if trace else _completion_texts(completion or ())
    return sum(text.count(BLOCK_MARKER) for text in texts)


class BlockLog:
    """Counts blocked rollouts across processes.

    TRL calls the reward function in the rollout process, so a counter in memory never reaches
    the trainer. The reward function appends one line per blocked rollout to a file in the run
    directory; the trainer side `drain()`s the new lines into the `guard_blocked_rollouts`
    metric. Appends of one short line are atomic on POSIX.
    """

    def __init__(self, path: str | Path) -> None:
        self.path = Path(path)
        self._offset = 0

    def record(self, n_blocked_calls: int) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        with self.path.open("a", encoding="utf-8") as handle:
            handle.write(f"{n_blocked_calls}\n")

    def drain(self) -> int:
        """Blocked rollouts recorded since the previous `drain()`."""
        if not self.path.is_file():
            return 0
        with self.path.open("rb") as handle:
            handle.seek(self._offset)
            data = handle.read()
        complete = data[: data.rfind(b"\n") + 1]  # ignore a half-written last line
        self._offset += len(complete)
        return len(complete.splitlines())
