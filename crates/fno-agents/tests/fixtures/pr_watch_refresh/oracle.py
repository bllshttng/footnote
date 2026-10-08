#!/usr/bin/env python3
"""The capture-mode oracle for the pr-watch refresh parity test.

Runs the Python leg (`refresh_watcher` + `heal_status_line`, the body the
`fno do pr watch refresh` leaf used before its forward) over the same
fixture the Rust verb reads, with the tick-in-flight answer and the
launchctl steps injected through the same env pins the Rust side reads.
Captured goldens freeze its bytes; the Python function survives only for
its remaining callers (`groom.py`, the `heal` leaf) until wave 6, so
capture mode keeps working until the package dies.
"""

import argparse
import json
import os
from pathlib import Path
from unittest import mock


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--case", required=True, help="fixture case directory")
    ap.add_argument("--force-bounce", action="store_true")
    args = ap.parse_args()

    from fno.config import load_settings
    from fno.pr_watch import _install as m

    agents_dir = Path(args.case) / "LaunchAgents"

    scripted = json.loads(os.environ.get("FNO_TEST_PR_WATCH_LAUNCHCTL", "[]"))

    def fake_launchctl(*a, timeout_s=10.0):
        if not scripted:
            raise AssertionError("unexpected launchctl call")
        rc, timed = scripted.pop(0)
        return rc, timed

    tick_pid = os.environ.get("FNO_TEST_PR_WATCH_TICK_PID") or None

    settings = load_settings()
    if not settings.pr_watch.enabled:
        print("pr-watch: disabled; nothing to refresh.")
        return 0
    with mock.patch.object(
        m, "_run_launchctl_timed", fake_launchctl
    ), mock.patch.object(
        m,
        "_tick_in_flight",
        lambda run=None: int(tick_pid) if tick_pid and int(tick_pid) > 0 else None,
    ):
        msg, _rc = m.refresh_watcher(
            launch_agents_dir=agents_dir,
            fno_binary="/usr/local/bin/fno",
            interval=settings.pr_watch.interval_seconds,
            defer_when_ticking=True,
            caller="refresh",
            force_bounce=args.force_bounce,
        )
    print(f"pr-watch refresh: {msg}")
    print(m.heal_status_line())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
