"""Transport for the ``spawn-axes`` verb: one JSON payload in, one parsed
answer out; a missing or failing owner raises SpawnAxesUnavailable - a
named refusal, never a silent spawn."""

from __future__ import annotations

from typing import Any

from fno.rust_binary import VerbUnavailable, verb_call


class SpawnAxesUnavailable(VerbUnavailable):
    """The fno-agents binary is missing, failed, or answered malformed JSON."""


def spawn_axes_call(payload: dict[str, Any]) -> dict[str, Any]:
    """One subprocess round-trip: JSON payload in, parsed JSON answer out."""
    return verb_call("spawn-axes", payload, SpawnAxesUnavailable)


def _answer_or_raise(answer: dict[str, Any], field: str) -> Any:
    """One ask's answer, a named refusal, or unavailable when the binary
    predates the field (a stale answer must never read as empty)."""
    if answer.get("refused"):
        from fno.agents.dispatch import DispatchAskError

        raise DispatchAskError(answer["refused"], exit_code=2)
    value = answer.get(field)
    if value is None:
        raise SpawnAxesUnavailable(
            f"spawn-axes answered no {field}; the fno-agents binary predates "
            "this ask - run `fno doctor update --rust`"
        )
    return value


def keeper_posture(harness: str, lane: str) -> list[str]:
    """The lane-default permission tokens for one harness lane, from the
    Rust owner; a non-empty note prints once to stderr as ``agy posture:
    <effective> (<source>) - <note>``."""
    ask = {"harness": harness, "lane": lane}
    answer = spawn_axes_call({"keeper_posture": ask})
    tokens = _answer_or_raise(answer, "tokens")
    note = answer.get("note") or ""
    if note:
        import sys
        head = f"agy posture: {answer.get('effective')} ({answer.get('source')})"
        print(f"{head} - {note}", file=sys.stderr)
    return [str(t) for t in tokens]




