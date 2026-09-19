"""The permission axis on the codex thread lane: does a configured or typed
mode ride the lane natively, per the capability table, through the one Rust
vocabulary (crates/fno-agents/src/codex_posture.rs)."""
from __future__ import annotations

import sys


def codex_thread_lane_carries(harness, substrate, once, mode):
    """The codex thread lane carries a mapped mode when the capability table
    declares the lane AND the mode resolves in codex's own words. An
    unavailable owner reads false, which degrades to the caller's refusal -
    never a guessed yes."""
    if harness != "codex" or substrate not in ("thread", "bg") or once or not mode:
        return False
    from fno.rust_binary import VerbUnavailable, verb_call

    try:
        answer = verb_call(
            "permission-tokens",
            {"provider": harness, "mode": mode, "substrate": substrate},
            VerbUnavailable,
        )
    except VerbUnavailable:
        return False
    if answer.get("refusal"):
        print(answer["refusal"], file=sys.stderr)
        return False
    return bool(answer.get("mappable"))
