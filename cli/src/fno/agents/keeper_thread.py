"""Which keeper-lane harness a thread spawn is, and what it refuses.

One loop here, one ``[harness.<name>.keeper]`` row in the capability contract.
The loop owns the shape, the row owns the sentence: a refusal is runtime text
an operator reads, so it stays per-harness and verbatim. Field meanings and
the measurement behind each row: docs/architecture/thread-lanes.md.
"""
from __future__ import annotations

from pathlib import Path
from typing import Any, Callable, Optional

#: Every launch axis a spawn can carry, as (flag an operator types, the row's
#: ``carries`` name). A row that omits the name refuses the flag; a row that
#: names an axis ``_lane_b_thread_spawn`` has no parameter for raises a
#: TypeError naming it, which only an operator override can produce.
LAUNCH_AXES: tuple[tuple[str, str], ...] = (
    ("--model", "model"),
    ("--yolo", "yolo"),
    ("--permission-mode", "permission_mode"),
    ("--effort", "effort"),
    ("--add-dir", "add_dir"),
    ("--role", "launch_role"),
    ("--agent", "agent"),
    ("--tools", "tools"),
    ("--deny-tools", "deny_tools"),
)


def _agy_finish(argv: list[str], cwd: Path) -> list[str]:
    """agy takes no extra tokens; it needs the folder TRUSTED before launch. A
    resume id skips the mint that upserts the grant, so the upsert happens here
    too. A refused write is not fatal: the seed submit answers the modal."""
    _trust_agy_folder(cwd)
    return argv


#: The one completion per harness the contract cannot express. Bare ``pi``
#: defaults to provider google, so the pair is always appended.
_FINISH_ARGV: dict[str, Callable[[list[str], Path], list[str]]] = {
    "pi": lambda argv, cwd: [*argv, *_pi_provider_model()],
    "agy": _agy_finish,
}


def keeper_arm(harness: str) -> Optional[dict]:
    """This harness's keeper row, or ``None`` when it has no keeper lane."""
    from fno.agents.harness_map import _HARNESS_CAPS

    return (_HARNESS_CAPS.get(harness) or {}).get("keeper")


def _pi_provider_model() -> list[str]:
    from fno.agents.harnesses.pi import pi_model, pi_provider

    return ["--provider", pi_provider(), "--model", pi_model()]


def _trust_agy_folder(cwd: Path) -> bool:
    from fno.agents.mux_spawn import _ensure_agy_folder_trusted

    return _ensure_agy_folder_trusted(cwd)


def mint_session_id(harness: str, cwd: Path, requested: Optional[str]) -> Optional[str]:
    """The harness-minted id for a keeper thread, or ``None`` for the
    caller-assigned default.

    The row's ``session_binding.strategy`` says whether a mint is REQUIRED and
    the mint itself is per-harness code, so the two are checked against each
    other below. Either way the id exists before any worker starts. A requested
    id (``spawn --resume``) is VALIDATED, never minted: a truncated one names a
    rival conversation.
    """
    if harness == "cursor-agent":
        from fno.agents.harnesses.cursor_agent import _require_chat_id, create_chat

        return _require_chat_id(requested) if requested is not None else create_chat(cwd)
    if harness == "agy":
        from fno.agents.harnesses.agy import create_conversation, require_conversation_id

        if requested is not None:
            return require_conversation_id(requested)
        # The mint runs a real turn in the spawn's own cwd, so an untrusted
        # folder would put a modal in front of the mint too.
        _trust_agy_folder(Path(cwd))
        return create_conversation(cwd)
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


def complete_launch_argv(
    harness: str,
    argv: list[str],
    *,
    cwd: Path,
    model: Optional[str],
    yolo: bool,
    permission_mode: Optional[str],
    add_dir: Optional[str],
    effort: Optional[str],
) -> list[str]:
    """The declared create form plus the axes this harness's PANE arm appends.
    One ORDER serves every lane: flag order is not how a binary launches."""
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import effort_tokens, permission_pane_tokens
    from fno.agents.writable_dirs import add_dir_tokens, worker_writable_dirs

    arm = keeper_arm(harness)
    if arm is None:
        return argv
    bypass = arm.get("bypass_flag")
    if arm.get("bypass_always") and bypass:
        argv = [*argv, bypass]
    if permission_mode:
        argv = [*argv, *permission_pane_tokens(harness, permission_mode)]
    elif yolo and bypass and not arm.get("bypass_always"):
        argv = [*argv, bypass]
    if arm.get("takes_model") and model:
        argv = [*argv, "--model", model]
    if arm.get("takes_effort") and effort:
        argv = [*argv, *effort_tokens(harness, effort)]
    if arm.get("takes_add_dir"):
        argv = [
            *argv,
            *add_dir_tokens(
                harness,
                add_dir,
                worker_writable_dirs(cwd),
                unsupported=lambda flag: (_ for _ in ()).throw(
                    DispatchAskError(
                        f"{flag} is not supported on the {harness} thread lane",
                        exit_code=2,
                    )
                ),
            ),
        ]
    finish = _FINISH_ARGV.get(harness)
    return finish(argv, cwd) if finish is not None else argv


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

    carries = tuple(arm.get("carries") or ())
    refused = next(
        (flag for flag, axis in LAUNCH_AXES if axis not in carries and options.get(axis)),
        None,
    )
    if refused is not None:
        raise DispatchAskError(
            f"{refused} is not supported on the {harness} thread lane; "
            "drop it or use --substrate pane",
            exit_code=2,
        )
    resume_session_id = options.get("resume_session_id")
    if resume_session_id and arm.get("resume_refusal"):
        raise DispatchAskError(
            f"--resume {resume_session_id} {arm['resume_refusal']}", exit_code=2
        )

    lane_kwargs: dict[str, Any] = {key: options.get(key) for key in carries}
    if "yolo" in lane_kwargs:
        lane_kwargs["yolo"] = bool(lane_kwargs["yolo"])
    receipt = _lane_b_thread_spawn(
        name=name,
        harness=harness,
        cwd=cwd,
        lock_timeout=lock_timeout,
        **lane_kwargs,
    )
    session_id = receipt["session_id"]
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
