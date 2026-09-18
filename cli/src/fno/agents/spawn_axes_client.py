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
    """The owner's answer for one ask, or a named refusal.

    A `refused` key is the owner's fail-closed decision (re-raised in the
    caller's own error type); a MISSING answer field means the binary that
    answered predates the field, which must read as unavailable - a stale
    binary answering the old shape must never read as an empty posture."""
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
    """The launch permission tokens for one harness lane, from the Rust owner.

    A non-empty note prints once to stderr as
    ``agy posture: <effective> (<source>) - <note>`` so every launch names
    the posture it ran with. Transport only: the vocabulary lives in
    crates/fno-agents/src/codex_posture.rs, the rule in agy_launch.rs."""
    answer = spawn_axes_call(
        {
            "keeper_posture": {
                "harness": harness,
                "lane": lane,
                "permission_mode": permission_mode or "",
                "yolo": bool(yolo),
            }
        }
    )
    tokens = _answer_or_raise(answer, "tokens")
    note = answer.get("note") or ""
    if note:
        import sys

        print(
            f"agy posture: {answer.get('effective')} "
            f"({answer.get('source')}) - {note}",
            file=sys.stderr,
        )
    return [str(t) for t in tokens]


def agy_mint_argv(
    model: str | None,
    effort: str | None,
    permission_mode: str | None,
    yolo: bool,
) -> list[str]:
    """The agy conversation-mint argv, from the Rust owner: the measured
    print turn carrying the spawn's selected axes."""
    answer = spawn_axes_call(
        {
            "agy_mint": {
                "model": model or "",
                "effort": effort or "",
                "permission_mode": permission_mode or "",
                "yolo": bool(yolo),
            }
        }
    )
    argv = _answer_or_raise(answer, "argv")
    return [str(t) for t in argv]
