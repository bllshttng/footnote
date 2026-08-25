"""The one place that says what ``fno agents rm`` is about to destroy.

On the harnesses that have one, ``rm`` tears down the harness's own session
record before it touches the fno registry, and that record IS the resume
handle. The transcript survives on disk, so the loss is reversible with ``fno
agents adopt`` -- but nothing in the verb, its help, or its receipt used to say
either half.

This module is shared rather than inlined because ``rm`` reaches the teardown
through two runtimes: the Rust binary serves ``fno agents rm`` by default (an
installed binary wins at the ``rust_runtime`` seam), and the Python
``dispatch.rm_agent`` serves a forced ``FNO_AGENTS_RUNTIME=python``, a
development checkout with no installed binary, and the documented
wedged-daemon escape hatch that imports ``rm_agent`` directly. A notice on one
of those paths is decorative; the seam gate below is what covers both.
"""
from __future__ import annotations

import os
import sys
from typing import IO, Optional

#: Verbatim in the notice and asserted on by the round-trip test. A caller who
#: greps a receipt for the recovery verb finds this exact string.
ADOPT_VERB = "fno agents adopt"

#: Set by the seam once it has written the notice, read by :func:`rm_agent` so
#: the Python route does not print the same block twice. The env is the carrier
#: because the seam either execs the Rust binary (which never reads it) or
#: falls through to the Python dispatch in this same process. Same hazard the
#: seam's own comments record for the env-scrub spawn warning.
NOTICE_SHOWN_ENV = "FNO_RM_NOTICE_SHOWN"

#: Opt out of the confirmation prompt WITHOUT opting into ``--force``. Those are
#: two different decisions: ``--force`` also drops a live row and leaves a named
#: orphan when teardown fails. An operator reaping several rows should be able
#: to stop being asked without silently taking on the other half.
ASSUME_YES_ENV = "FNO_RM_ASSUME_YES"

#: Harnesses whose ``rm`` actually tears a session record down, so a reap really
#: does forfeit a resume handle. opencode is registry-only by design (removing
#: its session would take the child sessions and the whole message history with
#: it), and gemini has no teardown arm at all. Warning for those two would claim
#: a loss that does not happen -- the same unfounded-report defect this module
#: exists to fix, pointed the other way.
TEARDOWN_HARNESSES = frozenset({"claude", "codex"})


def resume_handle_notice(name: str, harness: str, handle: str) -> str:
    """The stderr block warning that a reap forfeits a resume handle.

    A full harness session UUID is the durable recovery handle. A Claude
    short-id remains a best-effort lookup only while the harness evidence that
    resolves it still exists.
    """
    is_short_id = len(handle) == 8 and all(
        char in "0123456789abcdef" for char in handle
    )
    if is_short_id:
        recovery_line = (
            "      This short id is a best-effort lookup only while durable "
            "harness evidence still resolves it.\n"
        )
    else:
        recovery_line = (
            "      Keep this full harness session UUID as the resume handle and "
            "recovery handle.\n"
        )
    return (
        f"WARN: removing the {harness} session record for {name} ({handle}).\n"
        "      The transcript stays on disk.\n"
        + recovery_line
        + f"      Reverse it with: {ADOPT_VERB} {handle} --cross-project\n"
    )


def forfeits_resume_handle(entry) -> bool:
    """True when removing ``entry`` really does tear a session record down."""
    return (getattr(entry, "harness", "") or "") in TEARDOWN_HARNESSES


def resume_handle_for(entry) -> Optional[str]:
    """The id ``adopt`` would take back for ``entry``, or None when there is none.

    Prefers the full ``harness_session_id`` over the 8-hex ``short_id``: adopt
    accepts either, and only the full id is collision-free. A codex id is
    time-prefixed, so its first eight collide across same-window sessions --
    naming the short one could point at a sibling.

    A row with neither id (a corrupted row, or a mux pane worker that never got
    a ``short_id``) has no handle to forfeit, so it gets no notice rather than
    one naming an empty string.
    """
    handle = (getattr(entry, "harness_session_id", "") or "").strip()
    if handle:
        return handle
    handle = (getattr(entry, "short_id", "") or "").strip()
    return handle or None


def lookup_row(name: str):
    """Resolve ``name`` the way ``rm`` itself resolves it, or None.

    Uses the shared ``resolve_agent`` rather than an exact-name scan, because
    ``rm`` resolves through ``_resolve_lifecycle_target`` and therefore accepts
    a name, a full session id, or a short handle. An exact-name lookup here
    would leave the guard silent for ``fno agents rm 0a6e775f`` -- the very
    short-id spelling this change teaches operators to use.

    Returns ``None`` on any failure: an unreadable registry, a torn one, an
    ambiguous token. A notice is an advisory, so a read that cannot answer
    degrades to silence and lets the real verb produce the real error.
    """
    try:
        from fno.agents.registry import resolve_agent

        resolved = resolve_agent(name)
    except Exception:  # noqa: BLE001 - advisory read; refusals belong to rm
        return None
    return getattr(resolved, "entry", None)


def warn_and_confirm(
    name: str,
    *,
    force: bool = False,
    stderr: Optional[IO[str]] = None,
    stdin: Optional[IO[str]] = None,
    env: Optional[dict] = None,
) -> bool:
    """Write the notice for ``name`` and decide whether the reap may proceed.

    Returns True to proceed, False only when an operator answered no at an
    interactive prompt.

    The prompt is offered ONLY when BOTH stdin and stderr are a TTY, ``force``
    is unset, and :data:`ASSUME_YES_ENV` is unset. A non-interactive caller is
    warned and proceeds: ``fno agents watchdog --apply-all`` and ``fno backlog
    groom`` reap rows unattended, and a blocking prompt there wedges the fleet
    sweep. The receipt is what carries the reversal in that case.

    Requiring stderr too is not belt-and-braces. ``dispatch-node.sh`` and
    ``spawn.sh`` both call ``fno agents rm ... >/dev/null 2>&1`` with stdin
    inherited, so on an interactive terminal a stdin-only check would block on
    a question whose text went to /dev/null -- a silent hang, from a guard
    meant to prevent a silent loss. A prompt nobody can read is not consent to
    ask for. Same idiom the dispatch layer already uses for its own interactive
    check.
    """
    err = stderr if stderr is not None else sys.stderr
    stream = stdin if stdin is not None else sys.stdin
    environ = env if env is not None else os.environ

    entry = lookup_row(name)
    if entry is None or not forfeits_resume_handle(entry):
        return True
    handle = resume_handle_for(entry)
    if handle is None:
        return True

    err.write(
        resume_handle_notice(
            getattr(entry, "name", name), getattr(entry, "harness", "?"), handle
        )
    )
    err.flush()
    environ[NOTICE_SHOWN_ENV] = "1"

    interactive = bool(getattr(stream, "isatty", lambda: False)()) and bool(
        getattr(err, "isatty", lambda: False)()
    )
    if force or environ.get(ASSUME_YES_ENV) or not interactive:
        return True

    # Name the quiet opt-out in the prompt itself. An operator batch-reaping
    # who is only trying to stop being asked must not reach for --force, which
    # also drops a live row and leaves a named orphan on teardown failure.
    err.write(
        f"Remove {getattr(entry, 'name', name)} anyway? "
        f"[y/N] (set {ASSUME_YES_ENV}=1 to stop asking) "
    )
    err.flush()
    try:
        answer = stream.readline()
    except (OSError, ValueError):
        # A closed or unreadable stdin is not consent.
        answer = ""
    return answer.strip().lower() in ("y", "yes")
