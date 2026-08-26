#!/usr/bin/env python3
"""Run bounded positive probes for decisions with known shipped enforcement."""

from __future__ import annotations

from pathlib import Path

from fno.decide.graduation import evaluate_graduation


ROOT = Path(__file__).resolve().parents[2]
KNOWN_PROBES = (
    {
        "decision_id": "d-1ca0e711",
        "graduation": {
            "kind": "enforced",
            "artifact": (
                "test:cli/tests/integration/test_graph_cli.py::"
                "test_new_p0_requires_breaking_acknowledgment"
            ),
        },
    },
)


def main() -> int:
    retired = 0
    for row in KNOWN_PROBES:
        result = evaluate_graduation(row, root=ROOT)
        decision_id = result["decision_id"]
        marker = result.get("graduation_checked", "probe_not_run")
        print(f"graduation_checked decision={decision_id} marker={marker}")
        if result["status"] == "retired":
            print(f"graduation_retired decision={decision_id}")
            retired += 1
    print(f"graduation_scan_complete probes={len(KNOWN_PROBES)} retired={retired}")
    return 0 if retired == len(KNOWN_PROBES) else 1


if __name__ == "__main__":
    raise SystemExit(main())
