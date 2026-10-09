#!/usr/bin/env python3
"""The capture-mode oracle for the pr-watch install parity test.

Runs the Python install leg (``_install.install`` composed with
``heal_status_line``, the exact body the typer leaf carried before the
forward) over the same fixture the Rust verb reads, with launchctl answered
by the stub both legs share through PATH. Captured goldens freeze its bytes;
the Python functions were deleted in the same change that landed the port,
so capture mode refuses once the leg is gone.
"""

import argparse
import os
from pathlib import Path


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--case", required=True, help="fixture case directory")
    ap.add_argument("--mode", choices=["install", "ensure"], default="install")
    ap.add_argument("--interval", type=int, default=0)
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--no-activate", action="store_true")
    args = ap.parse_args()

    from fno.pr_watch import _install as m

    agents_dir = Path(os.environ["FNO_TEST_PR_WATCH_LAUNCH_AGENTS_DIR"])
    fno_binary = os.environ.get("FNO_TEST_FNO_BINARY", "fno-py")

    # The typer leaf resolved the config interval before calling the module
    # leg; the oracle composes the same way, so the frozen bytes are the
    # verb's observable contract and not the module default.
    interval = args.interval
    if interval <= 0:
        from fno.config import load_settings

        interval = load_settings().pr_watch.interval_seconds

    if args.mode == "ensure":
        print(
            m.ensure_activated(
                launch_agents_dir=agents_dir,
                fno_binary=fno_binary,
                interval=interval,
            )
        )
        return 0

    m.install(
        launch_agents_dir=agents_dir,
        fno_binary=fno_binary,
        interval=interval,
        dry_run=args.dry_run,
        activate=not args.no_activate,
    )
    # A fresh install sees the healer's arm state beside the watcher's.
    print(m.heal_status_line())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
