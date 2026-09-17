"""The pr-watch tick's heal phase: the gate in front of the Rust drive loop.

The loop lives in crates/fno-agents/src/heal.rs (`--all --apply`); this module
resolves the binary and runs it once per project root, each passed with
``--cwd`` because launchd starts the tick in ``/``.
"""
from __future__ import annotations

import logging
from pathlib import Path
from typing import Any, Callable

log = logging.getLogger(__name__)

#: Belt over a wedged spawn, not a run bound: the phase passes ``--detach``,
#: the binary answers in milliseconds and the drive loop bounds each remedy
#: itself; the tick's own SIGALRM deadline bounds the phase.
_DRIVE_TIMEOUT_S = 30


def run_heal_phase(
    settings: Any,
    roots: list[Path],
    *,
    resolve_binary: Callable[[], Any] | None = None,
    run: Callable[..., Any] | None = None,
) -> str:
    """Run ``fno-agents pr-heal --all --apply`` once per root.

    Unarmed (``auto_heal.enabled`` falsy or the block absent) answers
    ``"unarmed"`` without resolving the binary: the launchd hot path pays
    nothing. Armed, returns ``"ran"``, ``"no-binary"``, or ``"no-roots"``;
    one failing root logs and never stops the rest.
    """
    if not getattr(getattr(settings, "auto_heal", None), "enabled", False):
        return "unarmed"
    if resolve_binary is None:
        from fno.rust_binary import resolve_binary as _resolve
        resolve_binary = _resolve
    binary = resolve_binary()
    if binary is None:
        log.warning("pr-watch: heal phase: the fno-agents binary was not found; reinstall fno or set FNO_AGENTS_BIN")
        return "no-binary"
    if not roots:
        # Never report a run that did not happen: an armed tick with no
        # project roots mints no pr_heal_tick row, and the log must not read
        # as though the loop executed.
        return "no-roots"
    if run is None:
        import subprocess

        run = subprocess.run
    for root in roots:
        try:
            proc = run(
                [
                    str(binary),
                    "pr-heal",
                    "--all",
                    "--apply",
                    "--detach",
                    "--cwd",
                    str(root),
                ],
                check=False,
                timeout=_DRIVE_TIMEOUT_S,
            )
            # 0..3 are drive-loop verdicts (clean, escalations remain,
            # in-flight, refusal). 4 is a read error and 127 a missing binary:
            # a binary too old to know --detach fails this way in milliseconds
            # and would otherwise read as "ran" with zero healing done.
            code = getattr(proc, "returncode", 0)
            if code not in (0, 1, 2, 3):
                log.warning(
                    "pr-watch: heal drive loop for %s exited %s "
                    "(4 = read error, 127 = binary missing; a stale binary "
                    "that lacks --detach fails this way; run `fno doctor`)",
                    root,
                    code,
                )
        except Exception as exc:  # noqa: BLE001 - one root never stops the rest
            log.warning("pr-watch: heal drive loop failed for %s: %s", root, exc)
    return "ran"
