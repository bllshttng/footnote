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

import multiprocessing as mp
import time
from pathlib import Path

import pytest

from fno.claims.lanes import acquire_lane_slot, active_lane_count


def _try_acquire_lane(root_str: str, max_lanes: int, lane_id: str, result_queue) -> None:
    """Child worker: acquire a lane slot with a distinct lane_id."""
    try:
        claim = acquire_lane_slot(
            max_lanes=max_lanes, lane_id=lane_id, root=Path(root_str)
        )
        if claim is None:
            result_queue.put(("capped", lane_id, None))
        else:
            result_queue.put(("won", lane_id, claim.key))
    except Exception as exc:  # pragma: no cover - surfaced as a failure
        result_queue.put(("error", lane_id, repr(exc)))


def _run_lane_race(root: Path, max_lanes: int, n_workers: int) -> list[tuple]:
    ctx = mp.get_context("spawn")
    queue = ctx.Queue()
    procs = [
        ctx.Process(
            target=_try_acquire_lane,
            args=(str(root), max_lanes, f"node-{i}", queue),
        )
        for i in range(n_workers)
    ]
    for p in procs:
        p.start()

    outcomes: list[tuple] = []
    deadline = time.monotonic() + 10.0
    while len(outcomes) < n_workers and time.monotonic() < deadline:
        try:
            outcomes.append(queue.get(timeout=0.5))
        except Exception:
            continue
    for p in procs:
        p.join(timeout=10)
    return outcomes


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
    assert active_lane_count(root=tmp_path) == max_lanes, f"trial {trial}: count drift"


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
    assert active_lane_count(root=tmp_path) == max_lanes
