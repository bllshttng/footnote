"""The one place that says what ``fno agents rm`` is about to destroy.

``rm`` tears down the harness's own session record before it touches the fno
registry, and that record IS the resume handle. The transcript survives on
disk, so the loss is reversible with ``fno agents adopt`` -- but nothing in the
verb, its help, or its receipt used to say either half.

This module is shared rather than inlined because ``rm`` reaches the teardown
through two runtimes: the Rust binary serves ``fno agents rm`` by default (an
installed binary wins at the ``rust_runtime`` seam), and the Python
``dispatch.rm_agent`` serves a forced ``FNO_AGENTS_RUNTIME=python``, a
development checkout with no installed binary, and the documented
wedged-daemon escape hatch that imports ``rm_agent`` directly. A notice on one
of those paths is decorative; the seam gate below is what covers both.
"""
from __future__ import annotations

import sys
from typing import IO, Optional

#: Verbatim in the notice and asserted on by the round-trip test. A caller who
#: greps a receipt for the recovery verb finds this exact string.
ADOPT_VERB = "fno agents adopt"


def resume_handle_notice(name: str, harness: str, handle: str) -> str:
    """The stderr block warning that a reap forfeits a resume handle.

    ``handle`` is whatever ``adopt`` accepts back: a claude row's 8-hex
    ``short_id``, or a harness session id for the stores adopt also searches.
    """
    return (
        f"WARN: removing the {harness} session record for {name} ({handle}).\n"
        "      That record is the resume handle. The transcript stays on disk.\n"
        f"      Reverse it with: {ADOPT_VERB} {handle}\n"
    )


def resume_handle_for(entry) -> Optional[str]:
    """The id ``adopt`` would take back for ``entry``, or None when there is none.

    A row with neither id (a corrupted row, or a mux pane worker that never got
    a ``short_id``) has no handle to forfeit, so it gets no notice rather than
    one naming an empty string.
    """
    handle = (getattr(entry, "short_id", "") or "").strip()
    if handle:
        return handle
    handle = (getattr(entry, "harness_session_id", "") or "").strip()
    return handle or None


def lookup_row(name: str):
    """Resolve ``name`` to a registry row without importing the dispatch layer.

    Returns ``None`` on any failure -- an unreadable, torn, or absent registry.
    A notice is an advisory, so a read that cannot answer degrades to silence
    and lets the real verb produce the real error. Mirrors how
    ``peek._lookup_mux_pane`` swallows a torn registry rather than raising.
    """
    try:
        from fno.agents.registry import load_registry

        rows = load_registry()
    except Exception:
        return None
    for entry in rows:
        if getattr(entry, "name", None) == name:
            return entry
    return None


def warn_and_confirm(
    name: str,
    *,
    force: bool = False,
    stderr: Optional[IO[str]] = None,
    stdin: Optional[IO[str]] = None,
) -> bool:
    """Write the notice for ``name`` and decide whether the reap may proceed.

    Returns True to proceed, False only when an operator answered no at an
    interactive prompt.

    The prompt is offered ONLY when BOTH stdin and stderr are a TTY and
    ``force`` is unset. A non-interactive caller is warned and proceeds: ``fno
    agents watchdog --apply-all`` and ``fno backlog groom`` reap rows
    unattended, and a blocking prompt there wedges the fleet sweep. The receipt
    is what carries the reversal in that case.

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

    entry = lookup_row(name)
    if entry is None:
        return True
    handle = resume_handle_for(entry)
    if handle is None:
        return True

    err.write(resume_handle_notice(name, getattr(entry, "harness", "?"), handle))
    err.flush()

    interactive = bool(getattr(stream, "isatty", lambda: False)()) and bool(
        getattr(err, "isatty", lambda: False)()
    )
    if force or not interactive:
        return True

    err.write(f"Remove {name} anyway? [y/N] ")
    err.flush()
    try:
        answer = stream.readline()
    except (OSError, ValueError):
        # A closed or unreadable stdin is not consent.
        answer = ""
    return answer.strip().lower() in ("y", "yes")
