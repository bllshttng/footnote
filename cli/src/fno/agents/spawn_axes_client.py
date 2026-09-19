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


def keeper_posture(
    harness: str,
    lane: str,
    permission_mode: str | None,
    yolo: bool,
) -> list[str]:
    """The launch permission tokens for one harness lane, from the Rust
    owner; a non-empty note prints once to stderr as ``agy posture:
    <effective> (<source>) - <note>``."""
    ask = {
        "harness": harness,
        "lane": lane,
        "permission_mode": permission_mode or "",
        "yolo": bool(yolo),
    }
    answer = spawn_axes_call({"keeper_posture": ask})
    tokens = _answer_or_raise(answer, "tokens")
    note = answer.get("note") or ""
    if note:
        import sys

        head = f"agy posture: {answer.get('effective')} ({answer.get('source')})"
        print(f"{head} - {note}", file=sys.stderr)
    return [str(t) for t in tokens]


def agy_mint_argv(
    model: str | None,
    effort: str | None,
    permission_mode: str | None,
    yolo: bool,
) -> list[str]:
    """The agy conversation-mint argv, from the Rust owner."""
    ask = {
        "model": model or "",
        "effort": effort or "",
        "permission_mode": permission_mode or "",
        "yolo": bool(yolo),
    }
    argv = _answer_or_raise(spawn_axes_call({"agy_mint": ask}), "argv")
    return [str(t) for t in argv]


def pi_session_lookup(cwd: str, session_id: str) -> dict[str, Any]:
    """What a ``(cwd, session_id)`` pair resolves to in pi's session store,
    from the Rust owner: ``state`` is one of ``unknown|none|one|duplicate``,
    with ``files``, ``directory`` and ``reason`` as they carry."""
    ask = {"cwd": str(cwd), "session_id": session_id}
    return spawn_axes_call({"pi_session_lookup": ask})


def pi_route(
    model: str | None,
    effort: str | None,
    tools: str | None = None,
    deny_tools: str | None = None,
) -> list[str]:
    """The provider/model/effort tokens a pi launch carries, from the Rust
    owner; the route note prints once to stderr as
    ``pi route: <route_source> - <note>``."""
    ask = {
        "model": model or "",
        "effort": effort or "",
        "tools": tools or "",
        "deny_tools": deny_tools or "",
    }
    answer = spawn_axes_call({"pi_route": ask})
    tokens = _answer_or_raise(answer, "tokens")
    note = answer.get("note") or ""
    if note:
        import sys

        print(
            f"pi route: {answer.get('route_source')} - {note}",
            file=sys.stderr,
        )
    return [str(t) for t in tokens]
