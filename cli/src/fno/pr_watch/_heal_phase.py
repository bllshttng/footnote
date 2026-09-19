"""The pr-watch tick's heal phase: the gate in front of the Rust drive loop.

The loop lives in crates/fno-agents/src/heal.rs (`--all --apply`); this module
resolves the binary and runs it ONCE, carrying every project root via
repeated ``--cwd`` flags, because launchd starts the tick in ``/``.
"""
from __future__ import annotations

import logging
from pathlib import Path
from typing import Any, Callable

log = logging.getLogger(__name__)

#: Belt over a wedged spawn, not a run bound: the phase passes ``--detach``,
#: the binary answers in milliseconds and the drive loop bounds each remedy
#: and the loop itself (DRIVE_BUDGET) in Rust. 5s, not the old
#: 30: at 30 the belt equaled the heal cap, so one wedged root spent the
#: whole phase (18 of 50 measured ticks saturated at 30.1s against a 0.3s
#: median).
_DRIVE_TIMEOUT_S = 5


def run_heal_phase(
    settings: Any,
    roots: list[Path],
    *,
    resolve_binary: Callable[[], Any] | None = None,
    run: Callable[..., Any] | None = None,
) -> str:
    """Run ``fno-agents pr-heal --all --apply`` once, every root aboard.

    Unarmed (``auto_heal.enabled`` falsy or the block absent) answers
    ``"unarmed"`` without resolving the binary: the launchd hot path pays
    nothing. Armed, returns ``"ran"``, ``"no-binary"``, ``"no-roots"``, or
    ``"failed"`` (the one spawn failed or exited a non-verdict code).
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
    try:
        proc = run(
            [str(binary), "pr-heal", "--all", "--apply", "--detach"]
            + [x for root in roots for x in ("--cwd", str(root))],
            check=False,
            timeout=_DRIVE_TIMEOUT_S,
        )
        # 0..3 are drive-loop verdicts; 4/127 means a stale binary that
        # lacks --detach and would otherwise read as "ran" with no row.
        code = getattr(proc, "returncode", 0)
        if code not in (0, 1, 2, 3):
            log.warning(
                "pr-watch: heal drive loop over every root exited %s; run `fno doctor`",
                code,
            )
            return "failed"
        return "ran"
    except Exception as exc:  # noqa: BLE001 - one wedged spawn never stops a tick
        log.warning("pr-watch: heal drive loop failed: %s", exc)
        return "failed"
