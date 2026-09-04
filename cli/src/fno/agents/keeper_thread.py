"""Which keeper-lane harness a thread spawn is, and what it refuses.

Four harnesses reach ``_lane_b_thread_spawn`` (cursor-agent, pi, grok, agy)
and each wanted the same forty lines in ``dispatch_spawn``: refuse the options
this lane has no measured spelling for, refuse ``--once``, maybe refuse
``--resume``, call the lane driver, paste the seed against this TUI's own idle
paint, emit the event, return the receipt. Only the DATA differed, so the fifth
copy is a table row instead of a fifth block.

Refusal prose stays per-harness and verbatim: it is self-teaching runtime text,
and the sentence an operator reads is the only place some of these facts are
written down. The loop owns the SHAPE, the row owns the sentence.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Optional


@dataclass(frozen=True)
class KeeperArm:
    """One keeper-lane harness's spawn contract."""

    #: Why this lane has no one-shot form.
    once_refusal: str
    #: The launch axes this harness carries, named for the lane-driver kwargs.
    #: Every OTHER axis is refused by name rather than silently dropped, so
    #: this one tuple answers both questions and they cannot disagree.
    carries: tuple[str, ...]
    #: This TUI's own composer-idle paint; ``None`` keeps the seed default.
    ready_marker: Optional[bytes] = None
    #: Why ``spawn --resume`` is refused. ``None`` means it is carried.
    resume_refusal: Optional[str] = None
    #: Why headless is refused, when this harness reaches the seam on it at
    #: all. ``None`` means the seam already refused it.
    headless_refusal: Optional[str] = None
    #: Refuse ``--once`` and headless with ONE message (cursor-agent: they are
    #: the same absent lane and it has always said so in one sentence).
    once_and_headless_together: bool = field(default=False)
    #: The binary's never-prompt flag, appended under ``--yolo`` - or always,
    #: where an unattended keeper has nobody to answer a first approval.
    bypass_flag: Optional[str] = None
    bypass_always: bool = False
    #: Which launch axes ride the argv, mirroring what ``build_pane_argv``
    #: appends: the keeper hosts the same TUI the pane does.
    takes_model: bool = False
    takes_effort: bool = False
    takes_add_dir: bool = False
    #: One completion the fields cannot express: ``(argv, cwd) -> argv``.
    finish_argv: Optional[Callable[[list[str], Path], list[str]]] = None
    #: ``(regex, keys)`` for a TUI that can paint a blocking modal before its
    #: composer. A keeper has nobody to answer one, and a TUI behind an
    #: unanswered modal runs NOTHING while holding a live registry row.
    clear_modal: Optional[tuple[str, bytes]] = None


#: Every launch axis a spawn can carry, as (flag an operator types, the
#: ``carries`` name). A row that omits the name refuses the flag.
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

KEEPER_ARMS: dict[str, KeeperArm] = {
    # cursor-agent has no daemon and no bidirectional transport: the keeper
    # holds the TUI's pty so the thread survives supervisor death, and the chat
    # id is minted by `create-chat` (callee-minted-read-back) before the child
    # launches. The pane stays the attended substrate; the thread lane is the
    # dispatch one.
    "cursor-agent": KeeperArm(
        once_refusal=(
            "cursor-agent has no headless lane: --print is output-only "
            "(no --input-format, no rpc), so there is no one-shot form. "
            "Use the thread substrate for a persistent worker or "
            "--substrate pane for an attended one."
        ),
        once_and_headless_together=True,
        carries=("model", "yolo", "permission_mode", "add_dir", "resume_session_id"),
        # The declared form already carries --trust (an untrusted cwd refuses
        # with Workspace Trust Required and fno always spawns into a fresh
        # worktree). --trust is NOT the bypass axis; --force is.
        bypass_flag="--force",
        takes_model=True,
        takes_add_dir=True,
    ),
    # pi's keeper lane is the one the restart journey proved (wk-x61bc). Its
    # headless lane never reaches here: _check_spawn_harness refuses the
    # unmeasured stance first. The lane driver mints the caller-assigned
    # session id and appends pi's provider/model pair itself, so every option
    # that would ride another lane's argv is refused by name.
    "pi": KeeperArm(
        once_refusal=(
            "--once is not supported on the pi thread lane (it is "
            "persistent); pi has no one-shot lane - its headless stance "
            "is unmeasured"
        ),
        carries=(),
        # pi's subscription tag renders only once its model session is wired.
        ready_marker=b"(sub)",
        resume_refusal=(
            "is not supported on the pi thread lane yet; the keeper row "
            "resumes by name (fno agents ask/resume <name>). Refusing rather "
            "than silently spawning a fresh session."
        ),
        # Bare pi defaults to provider google and `--provider openai-codex`
        # without `--model` falls to a Bedrock model, so the pair is appended
        # always - the same completion build_pane_argv has always made.
        finish_argv=lambda argv, cwd: [*argv, *_pi_provider_model()],
    ),
    # grok rides the same keeper lane. The measurement behind its row:
    # `--session-id` adopts the caller-assigned uuid, `--resume` on a fresh
    # process recalls a prior turn across a SIGKILL.
    "grok": KeeperArm(
        once_refusal=(
            "--once is not supported on the grok thread lane (it is "
            "persistent); grok has no one-shot lane - its headless "
            "stance is unmeasured"
        ),
        carries=("model", "yolo", "permission_mode", "effort"),
        # grok's status bar mode hint, its measured idle marker.
        ready_marker=b"Shift+Tab:mode",
        # `--session-id` on an id that already exists is a grok REFUSAL ("must
        # not already exist"), never a resume; rendering the create form with a
        # used id would fail the spawn with grok's flag error instead of fno's
        # posture. Same stance as pi.
        resume_refusal=(
            "is not supported on the grok thread lane yet; the keeper row "
            "resumes by name (fno agents ask/resume <name>). Refusing rather "
            "than spawning a fresh session."
        ),
        bypass_flag="--always-approve",
        takes_model=True,
        takes_effort=True,
    ),
    # agy mints its own conversation id and adopts none from the command line,
    # so the lane driver reads one back from a print-mode turn before the TUI
    # launches - cursor-agent's shape. Model, effort, add-dir and the
    # permission axis are carried because the pane arm carries all four.
    "agy": KeeperArm(
        once_refusal=(
            "--once is not supported on the agy thread lane (it is "
            "persistent); agy's one-shot form is `agy -p`, whose lane is "
            "unmeasured"
        ),
        carries=(
            "model", "yolo", "permission_mode", "add_dir", "effort",
            "resume_session_id",
        ),
        # agy's hint bar, measured 2026-09-03 on 1.1.24 after the restored
        # transcript finishes painting.
        ready_marker=b"? for shortcuts",
        # agy's state_root_grant records the write-access MECHANISM per lane
        # (--add-dir on all three), never whether a lane has been run, so
        # SPAWN_HARNESSES membership passes agy through the seam's
        # unmeasured-stance check for headless too. Nothing has driven
        # `agy -p` as a worker, so the refusal is stated here rather than
        # inherited from the thread lane that WAS measured.
        headless_refusal=(
            "agy's headless lane is unmeasured: `agy -p` prints and exits, "
            "and nothing has run it as an fno worker. Use --substrate thread "
            "for a persistent worker or --substrate pane for an attended one."
        ),
        bypass_flag="--dangerously-skip-permissions",
        bypass_always=True,
        takes_model=True,
        takes_effort=True,
        takes_add_dir=True,
        # A folder agy does not trust puts a modal in front of the composer and
        # the keeper has nobody to answer it. The mint already upserted this
        # cwd; a caller-supplied resume id skips the mint, so the upsert has to
        # happen on the launch path too.
        finish_argv=lambda argv, cwd: (_trust_agy_folder(cwd), argv)[1],
        # agy asks about folder trust before its composer, and the file upsert
        # above does not always take. Enter accepts the highlighted default,
        # "Yes, I trust this folder" - the same answer the pane lane submits.
        clear_modal=(r"trust (?:this )?folder|do you trust", b"\r"),
    ),
}


def _pi_provider_model() -> list[str]:
    from fno.agents.harnesses.pi import pi_model, pi_provider

    return ["--provider", pi_provider(), "--model", pi_model()]


def _trust_agy_folder(cwd: Path) -> bool:
    from fno.agents.mux_spawn import _ensure_agy_folder_trusted

    return _ensure_agy_folder_trusted(cwd)


def mint_session_id(harness: str, cwd: Path, requested: Optional[str]) -> Optional[str]:
    """The harness-minted id for a keeper thread, or ``None`` for the
    caller-assigned default, per the row's ``session_binding.strategy``.

    A ``callee-minted-read-back`` harness mints for itself (cursor-agent
    through ``create-chat``, agy through a print-mode turn's JSON envelope);
    everyone else takes fno's UUIDv4. Either way the id exists before any
    worker starts. A caller-requested id (``spawn --resume``) is VALIDATED
    rather than minted: a truncated one is a picker or a rival conversation.
    """
    if harness == "cursor-agent":
        from fno.agents.harnesses.cursor_agent import _require_chat_id, create_chat

        return _require_chat_id(requested) if requested is not None else create_chat(cwd)
    if harness == "agy":
        from fno.agents.harnesses.agy import create_conversation, require_conversation_id

        if requested is not None:
            return require_conversation_id(requested)
        # The mint runs a real turn in the spawn's own cwd, so the trust upsert
        # has to happen before it: an untrusted folder puts a modal in front of
        # the mint too, not only the composer.
        _trust_agy_folder(Path(cwd))
        return create_conversation(cwd)
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

    ``build_pane_argv`` and this share one job under two hosts. One ORDER
    serves every lane, which agy's pane arm spells the other way round (effort
    before model): flag order is not how a binary launches.
    """
    from fno.agents.dispatch import DispatchAskError
    from fno.agents.mux_spawn import effort_tokens, permission_pane_tokens
    from fno.agents.writable_dirs import add_dir_tokens, worker_writable_dirs

    arm = KEEPER_ARMS.get(harness)
    if arm is None:
        return argv
    if arm.bypass_always and arm.bypass_flag:
        argv = [*argv, arm.bypass_flag]
    if permission_mode:
        argv = [*argv, *permission_pane_tokens(harness, permission_mode)]
    elif yolo and arm.bypass_flag and not arm.bypass_always:
        argv = [*argv, arm.bypass_flag]
    if arm.takes_model and model:
        argv = [*argv, "--model", model]
    if arm.takes_effort and effort:
        argv = [*argv, *effort_tokens(harness, effort)]
    if arm.takes_add_dir:
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
    if arm.finish_argv is not None:
        argv = arm.finish_argv(argv, cwd)
    return argv


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
    """Run one keeper-lane thread spawn, or return ``None`` for another lane.

    ``options`` carries the ``dispatch_spawn`` locals the rows name.
    """
    from fno.agents.dispatch import (
        DispatchAskError,
        SpawnResult,
        _emit_ev,
        _keeper_seed_submit,
        _lane_b_thread_spawn,
    )

    arm = KEEPER_ARMS.get(harness)
    if arm is None:
        return None
    if headless:
        if arm.once_and_headless_together:
            raise DispatchAskError(arm.once_refusal, exit_code=2)
        if arm.headless_refusal is not None:
            raise DispatchAskError(arm.headless_refusal, exit_code=2)
        # The seam already refused this harness on headless (an unmeasured
        # stance), so reaching here would mean a row and a stance disagree.
        return None
    if once:
        raise DispatchAskError(arm.once_refusal, exit_code=2)

    refused = next(
        (
            flag
            for flag, axis in LAUNCH_AXES
            if axis not in arm.carries and options.get(axis)
        ),
        None,
    )
    if refused is not None:
        raise DispatchAskError(
            f"{refused} is not supported on the {harness} thread lane; "
            "drop it or use --substrate pane",
            exit_code=2,
        )
    resume_session_id = options.get("resume_session_id")
    if resume_session_id and arm.resume_refusal is not None:
        raise DispatchAskError(
            f"--resume {resume_session_id} {arm.resume_refusal}", exit_code=2
        )

    lane_kwargs: dict[str, Any] = {key: options.get(key) for key in arm.carries}
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
        if arm.ready_marker is not None:
            extra["ready_marker"] = arm.ready_marker
        if arm.clear_modal is not None:
            extra["clear_modal"] = arm.clear_modal
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
