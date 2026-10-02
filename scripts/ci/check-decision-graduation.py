#!/usr/bin/env python3
"""Run bounded positive probes for decisions with known shipped enforcement.

The registry used to live in the deleted Python decide family; the port made
this gate its only consumer, so the registry and the one artifact lane it
declares (`test:`) live here. Every probe claims a shipped enforcement
artifact; this re-measures that claim, so deleting the enforcement a retired
ruling rests on turns red here instead of silently un-retiring it.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

REGISTERED_GRADUATION_PROBES: tuple[dict[str, str], ...] = (
    {
        "decision_id": "d-1ca0e711",
        "graduation": (
            "test:cli/tests/integration/test_graph_cli.py::"
            "test_new_p0_requires_breaking_acknowledgment"
        ),
    },
)


def evaluate(node_id: str, artifact: str, *, root: Path, timeout: int) -> dict[str, str]:
    """One `test:` probe: the nodeid passing is the enforcement present."""
    payload = artifact.removeprefix("test:")
    try:
        completed = subprocess.run(
            [sys.executable, "-m", "pytest", "-q", payload],
            cwd=root,
            timeout=timeout,
            capture_output=True,
            text=True,
            check=False,
        )
    except (OSError, ValueError) as exc:
        return {"marker": f"probe_error:{type(exc).__name__}"}
    except subprocess.TimeoutExpired:
        return {"marker": f"probe_timeout:{artifact}"}
    if completed.returncode != 0:
        return {"marker": f"test_failed:{payload}"}
    return {"marker": f"test_passed:{payload}"}


def main() -> int:
    root = Path(__file__).resolve().parents[2]
    retired = 0
    for row in REGISTERED_GRADUATION_PROBES:
        # A test probe pays cold pytest collection on a CI runner. The 30s
        # library default would read that as `probe_timeout` and fail the job
        # over runner speed rather than over the enforcement being gone.
        result = evaluate(
            row["decision_id"], row["graduation"], root=root, timeout=300
        )
        marker = result["marker"]
        print(f"graduation_checked decision={row['decision_id']} marker={marker}")
        if marker.startswith(("test_passed:", "marker_present:")):
            print(f"graduation_retired decision={row['decision_id']}")
            retired += 1
    total = len(REGISTERED_GRADUATION_PROBES)
    print(f"graduation_scan_complete probes={total} retired={retired}")
    return 0 if retired == total else 1


if __name__ == "__main__":
    raise SystemExit(main())
