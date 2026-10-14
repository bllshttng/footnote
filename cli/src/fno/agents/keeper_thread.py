"""Which keeper-lane harness a thread spawn is, and what it refuses.

One loop here, one ``[harness.<name>.keeper]`` row in the capability contract.
The loop owns the shape, the row owns the sentence: a refusal is runtime text
an operator reads, so it stays per-harness and verbatim. Field meanings and
the measurement behind each row: docs/architecture/thread-lanes.md.
"""
from __future__ import annotations

import os
import uuid
from pathlib import Path
from typing import Any, Optional

from fno import paths


def keeper_arm(harness: str) -> Optional[dict]:
    """This harness's keeper row, or ``None`` when it has no keeper lane."""
    from fno.agents.harness_map import _HARNESS_CAPS

    return (_HARNESS_CAPS.get(harness) or {}).get("keeper")


def trust_agy_folder(cwd: Path) -> bool:
    """agy needs the folder TRUSTED before launch; the upsert is idempotent.
    A refused write is not fatal: the seed submit answers the modal."""
    from fno.agents.mux_spawn import _ensure_agy_folder_trusted

    return _ensure_agy_folder_trusted(cwd)


def mint_session_id(
    harness: str,
    cwd: Path,
    requested: Optional[str],
) -> Optional[str]:
    """The harness-minted id for a keeper thread, or ``None`` for the
    caller-assigned default. The row's ``session_binding.strategy`` says
    whether a mint is REQUIRED and the mint itself is per-harness code. A
    requested id is VALIDATED, never minted. agy runs no mint turn: its row
    binds the conversation id at the session report, so a spawn pays no
    launch-time model call.
    """
    if harness == "cursor-agent":
        from fno.agents.harnesses.cursor_agent import _require_chat_id, create_chat

        return _require_chat_id(requested) if requested is not None else create_chat(cwd)
    if harness == "agy":
        from fno.agents.harnesses.agy import require_conversation_id

        return require_conversation_id(requested) if requested is not None else None
    from fno.agents.harness_map import capabilities

    binding = capabilities(harness).get("session_binding") or {}
    if binding.get("strategy") == "callee-minted-read-back":
        from fno.agents.dispatch import DispatchAskError

        raise DispatchAskError(
            f"{harness} declares session_binding.strategy = "
            "callee-minted-read-back but fno has no mint for it; the "
            "caller-assigned UUIDv4 fallback launches the keeper on an id the "
            "harness never adopts, and Identify reports that fabricated id",
            exit_code=2,
        )
    return requested


def _mint_thread_session_id(
    harness: str,
    cwd: Path,
    requested: Optional[str] = None,
) -> str:
    """The harness session id a keeper thread launches on, fixed BEFORE
    launch: :func:`mint_session_id` plus the caller-assigned UUIDv4 default."""
    minted = mint_session_id(harness, cwd, requested)
    return minted if minted is not None else str(uuid.uuid4())


def keeper_thread_spawn(
    *,
    harness: str,
    name: str,
    message: str,
    effective_message: Optional[str],
    cwd: Path,
    headless: bool,
    once: bool,
    options: dict,
    lock_timeout: float,
):
    """Run one keeper-lane thread spawn, or ``None`` for another lane.
    ``options`` carries the ``dispatch_spawn`` locals the rows name."""
    from fno.agents.dispatch import (
        DispatchAskError,
        SpawnResult,
        _emit_ev,
        _keeper_seed_submit,
        _lane_b_thread_spawn,
    )

    arm = keeper_arm(harness)
    if arm is None:
        return None
    if headless:
        if arm.get("once_and_headless_together"):
            raise DispatchAskError(arm["once_refusal"], exit_code=2)
        if arm.get("headless_refusal"):
            raise DispatchAskError(arm["headless_refusal"], exit_code=2)
        # The seam already refused this harness on headless (an unmeasured
        # stance), so reaching here would mean a row and a stance disagree.
        return None
    if once:
        raise DispatchAskError(arm["once_refusal"], exit_code=2)

    receipt = _lane_b_thread_spawn(
        name=name,
        harness=harness,
        cwd=cwd,
        lock_timeout=lock_timeout,
        passthrough=options.get("passthrough"),
    )
    session_id = receipt["session_id"]
    if options.get("promotion") is not None:
        from fno.agents.team_thread import settle_thread_promotion

        settle_thread_promotion(options["promotion"], name=name, cwd=cwd, session_id=session_id)
    if message.strip():
        # The seed rides the keeper paste: a Resize forces a repaint, the idle
        # marker off the stream proves the composer is up, and the echo of the
        # submitted line is the landing proof.
        extra: dict[str, Any] = {}
        if arm.get("ready_marker"):
            extra["ready_marker"] = arm["ready_marker"].encode("utf-8")
        modal = arm.get("clear_modal") or ()
        if modal:
            extra["clear_modal"] = (modal[0], modal[1].encode("utf-8"))
        _keeper_seed_submit(
            name=name,
            session_id=session_id,
            sock=Path(receipt["keeper_socket"]),
            message=message,
            **extra,
        )
    _emit_ev(
        "agent_ask_done",
        stage="dispatch",
        name=name,
        provider=harness,
        substrate="thread",
    )
    return SpawnResult(
        kind="created",
        name=name,
        provider=harness,
        short_id=session_id,
        effective_message=effective_message,
    )


def _lane_b_keeper_socket(name: str) -> Path:
    """``<state-root>/mux/threads/<name>.sock``, session-keyed beside the pane
    keepers' ``mux/panes/``. Derivation + override rules: the
    ``mux/threads`` row in docs/state-root-inventory.md."""
    override = os.environ.get("FNO_AGENTS_HOME")
    if override:
        return Path(override).expanduser().parent / "mux" / "threads" / f"{name}.sock"
    return paths.state_dir() / "mux" / "threads" / f"{name}.sock"
