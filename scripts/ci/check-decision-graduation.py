#!/usr/bin/env python3
"""Run bounded positive probes for decisions with known shipped enforcement."""

from __future__ import annotations

from pathlib import Path

from fno.decide.graduation import REGISTERED_GRADUATION_PROBES, evaluate_graduation


ROOT = Path(__file__).resolve().parents[2]


def main() -> int:
    retired = 0
    for row in REGISTERED_GRADUATION_PROBES:
        # A test probe pays cold pytest collection on a CI runner. The 30s
        # library default would read that as `probe_timeout` and fail the job
        # over runner speed rather than over the enforcement being gone.
        result = evaluate_graduation(row, root=ROOT, timeout=300)
        decision_id = result["decision_id"]
        marker = result.get("graduation_checked", "probe_not_run")
        print(f"graduation_checked decision={decision_id} marker={marker}")
        if result["status"] == "retired":
            print(f"graduation_retired decision={decision_id}")
            retired += 1
    total = len(REGISTERED_GRADUATION_PROBES)
    print(f"graduation_scan_complete probes={total} retired={retired}")
    return 0 if retired == total else 1


if __name__ == "__main__":
    raise SystemExit(main())
