#!/usr/bin/env python3
"""The capture-mode oracle for the pr-watch status parity test.

Runs the Python leg (`liveness_report_live` / `status`) over the same
fixture the Rust verb reads, with the load state injected the way the
Python tests monkeypatch `_launchctl_is_loaded`. Captured goldens freeze
its bytes; the Python leg was deleted in the same change that landed the
port, so capture mode refuses once the leg is gone.
"""

import argparse
import json
from pathlib import Path
from unittest import mock


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--case", required=True, help="fixture case directory")
    ap.add_argument("--loaded", default="1")
    ap.add_argument("--mode", choices=["json", "text"], default="json")
    args = ap.parse_args()

    from fno.pr_watch import _install as m

    loaded = args.loaded == "1"
    agents_dir = Path(args.case) / "LaunchAgents"
    with mock.patch.object(m, "_launchctl_is_loaded", lambda: loaded):
        if args.mode == "json":
            report = m.liveness_report_live(launch_agents_dir=agents_dir)
            print(json.dumps(report))
        else:
            m.status(launch_agents_dir=agents_dir)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
