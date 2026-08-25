from __future__ import annotations

import pytest

from fno.backlog.undispatched import classify_planned_unclaimed


def _node(node_id: str, **overrides) -> dict:
    node = {
        "id": node_id,
        "status": "ready",
        "plan_path": "/plans/ready.md",
        "priority": "p0",
        "domain": "code",
        "blocked_by": [],
    }
    node.update(overrides)
    return node


def test_ac1_hp_known_undispatched_node_is_named():
    receipt = classify_planned_unclaimed(
        [
            {
                "id": "x-known-undispatched",
                "status": "ready",
                "plan_path": "/plans/ready.md",
                "priority": "p0",
                "domain": "code",
                "blocked_by": [],
            }
        ],
        [],
    )

    assert receipt["entries_scanned"] == 1
    assert any(row["id"] == "x-known-undispatched" for row in receipt["rows"])


def test_ac2_err_malformed_graph_is_unknown_not_empty():
    with pytest.raises(ValueError, match="graph"):
        classify_planned_unclaimed({"nodes": []}, [])


@pytest.mark.parametrize(
    "overrides",
    [
        {"status": "in_progress"},
        {"pr_number": 123},
        {"batch": "batch-code"},
        {"blocked_by": ["x-blocker"]},
        {"type": "epic"},
    ],
)
def test_ac5_started_or_unsafe_rows_stay_out(overrides):
    entries = [_node("x-known-undispatched"), _node("x-excluded", **overrides)]
    receipt = classify_planned_unclaimed(entries, [])

    assert [row["id"] for row in receipt["rows"]] == ["x-known-undispatched"]


@pytest.mark.parametrize("state", ["live", "suspect", "stale", "corrupted"])
def test_ac5_any_claim_state_stays_out(state):
    receipt = classify_planned_unclaimed(
        [_node("x-known-undispatched"), _node("x-held")],
        [{"key": "node:x-held", "state": state}],
    )

    assert [row["id"] for row in receipt["rows"]] == ["x-known-undispatched"]


def test_ac3_observer_miss_is_prepended_to_normal_selection():
    from fno.backlog.undispatched import prepend_missed_rows

    observer = classify_planned_unclaimed(
        [_node("x-p0-missed"), _node("x-p2-normal", priority="p2")], []
    )
    merged, missed = prepend_missed_rows(
        [_node("x-p2-normal", priority="p2")], observer
    )

    assert [row["id"] for row in missed] == ["x-p0-missed"]
    assert [row["id"] for row in merged] == ["x-p0-missed", "x-p2-normal"]


def test_scope_filters_project_mission_roadmap_and_parent():
    entries = [
        _node("x-in", project="fno", mission_id="m1", roadmap_id="r1", parent="x-root"),
        _node("x-project", project="etl", mission_id="m1", roadmap_id="r1", parent="x-root"),
        _node("x-mission", project="fno", mission_id="m2", roadmap_id="r1", parent="x-root"),
        _node("x-roadmap", project="fno", mission_id="m1", roadmap_id="r2", parent="x-root"),
        _node("x-other-parent", project="fno", mission_id="m1", roadmap_id="r1", parent="x-other"),
        _node("x-root", project="fno", mission_id="m1", roadmap_id="r1", type="epic"),
    ]

    from fno.backlog.undispatched import classify_planned_unclaimed

    receipt = classify_planned_unclaimed(
        entries, [], project="fno", mission="m1", roadmap_id="r1", parent="x-root"
    )

    assert [row["id"] for row in receipt["rows"]] == ["x-in"]
