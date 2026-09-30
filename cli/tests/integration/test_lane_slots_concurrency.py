"""Multi-process cap enforcement for lane slots (x-d050, Locked Decision #7).

The central claim of parallel mode's concurrency design: the lane cap is
enforced by claim ATOMICITY, not a counted integer. If N dispatch ticks race
to acquire lanes with cap K, exactly K may win - a racy count-then-acquire
would let more than K through. These tests drive real-process contention on
one filesystem path to prove the invariant holds.

Slots are TTL-anchored, so a winner's process exiting does NOT free its slot
(a TTL claim within its window is LIVE regardless of PID) - the assertion is
deterministic without keeping winners alive.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

import pytest

from fno.rust_binary import resolve_binary

pytestmark = pytest.mark.dev_build


def _run_lane_race(root: Path, max_lanes: int, n_workers: int) -> list[tuple]:
    """Race n real `claim lane-acquire` processes on one claims root.

    Exit 0 = won (stdout JSON carries the slot key), exit 1 = capped.
    """
    binary = resolve_binary()
    assert binary is not None, "dev binary required for the race"
    env = {**os.environ, "FNO_CLAIMS_ROOT": str(root)}
    procs = [
        subprocess.Popen(
            [str(binary), "claim", "lane-acquire", "--lane", f"node-{i}",
             "--max-lanes", str(max_lanes), "--json"],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        for i in range(n_workers)
    ]
    outcomes: list[tuple] = []
    for i, proc in enumerate(procs):
        out, err = proc.communicate(timeout=30)
        if proc.returncode == 0:
            outcomes.append(("won", f"node-{i}", json.loads(out)["key"]))
        elif proc.returncode == 1:
            outcomes.append(("capped", f"node-{i}", None))
        else:  # pragma: no cover - surfaced as a failure
            outcomes.append(("error", f"node-{i}", err.strip()))
    return outcomes


def _live_lane_count(root: Path) -> int:
    binary = resolve_binary()
    assert binary is not None, "dev binary required for the count"
    out = subprocess.run(
        [str(binary), "claim", "lane-count", "--json"],
        env={**os.environ, "FNO_CLAIMS_ROOT": str(root)},
        capture_output=True, text=True, check=True,
    )
    return json.loads(out.stdout)["active_lanes"]


@pytest.mark.parametrize("trial", range(3))
def test_cap_holds_under_race_more_workers_than_slots(tmp_path, trial):
    """8 racers, cap 3: exactly 3 win, 5 are capped, and the count is 3."""
    max_lanes = 3
    outcomes = _run_lane_race(tmp_path, max_lanes=max_lanes, n_workers=8)
    wins = [o for o in outcomes if o[0] == "won"]
    capped = [o for o in outcomes if o[0] == "capped"]
    errors = [o for o in outcomes if o[0] == "error"]

    assert errors == [], f"trial {trial}: unexpected errors {errors}"
    assert len(wins) == max_lanes, f"trial {trial}: expected {max_lanes} winners, got {outcomes}"
    assert len(capped) == 8 - max_lanes, f"trial {trial}: expected {8 - max_lanes} capped, got {outcomes}"

    # Winners occupy DISTINCT slots (no two lanes share one).
    won_slots = {o[2] for o in wins}
    assert len(won_slots) == max_lanes, f"trial {trial}: winners collided on slots {[o[2] for o in wins]}"

    # The derived count matches the cap exactly.
    assert _live_lane_count(tmp_path) == max_lanes, f"trial {trial}: count drift"


@pytest.mark.parametrize("trial", range(3))
def test_exactly_max_workers_all_win(tmp_path, trial):
    """N racers, cap N: all win, one slot each, count == N."""
    max_lanes = 4
    outcomes = _run_lane_race(tmp_path, max_lanes=max_lanes, n_workers=max_lanes)
    wins = [o for o in outcomes if o[0] == "won"]
    errors = [o for o in outcomes if o[0] == "error"]
    assert errors == [], f"trial {trial}: errors {errors}"
    assert len(wins) == max_lanes, f"trial {trial}: expected all {max_lanes} to win, got {outcomes}"
    assert len({o[2] for o in wins}) == max_lanes, f"trial {trial}: slot collision {outcomes}"
    assert _live_lane_count(tmp_path) == max_lanes
