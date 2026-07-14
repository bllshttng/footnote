"""Unit tests for the graph->frontmatter mirror projection (Wave 1).

Covers `fno.plan._project.project_node_to_plan`: the one-way projection that
upserts a node's navigation fields (priority/type/blocked_by/project) into its
plan doc's frontmatter, reusing the byte-preserving _stamp read/write path.
"""
from __future__ import annotations

from pathlib import Path

from fno.plan._project import project_node_to_plan
from fno.plan._stamp import read_plan_file

_PLAN = """\
---
node: x-abcd
status: ready
created: 2026-07-08
size: M
type: feature
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 15
    reason: too many
---

# A plan

body text
"""


def _write_plan(tmp_path: Path, text: str = _PLAN) -> Path:
    p = tmp_path / "plan.md"
    p.write_text(text, encoding="utf-8")
    return p


def test_hp_projects_mirror_fields(tmp_path):
    plan = _write_plan(tmp_path)
    node = {"priority": "p1", "type": "feature", "blocked_by": ["x-1", "x-2"], "project": "fno"}

    assert project_node_to_plan(node, plan) is True

    _, fields, _ = read_plan_file(plan)
    assert fields["priority"] == "p1"
    assert fields["project"] == "fno"
    assert fields["blocked_by"] == ["x-1", "x-2"]


def test_idempotent_second_run_no_write(tmp_path):
    plan = _write_plan(tmp_path)
    node = {"priority": "p1", "blocked_by": ["x-1"], "project": "fno"}

    assert project_node_to_plan(node, plan) is True
    before = plan.read_text(encoding="utf-8")
    assert project_node_to_plan(node, plan) is False  # no diff -> no write
    assert plan.read_text(encoding="utf-8") == before


def test_none_scalar_never_overwrites(tmp_path):
    plan = _write_plan(tmp_path)
    # priority present but None -> skip; type absent from node -> skip.
    node = {"priority": None, "blocked_by": ["x-9"], "project": None}

    assert project_node_to_plan(node, plan) is True  # blocked_by changed
    _, fields, _ = read_plan_file(plan)
    assert "priority" not in fields  # never wrote `priority: None`
    assert "project" not in fields
    assert fields["blocked_by"] == ["x-9"]


def test_empty_blocked_by_clears_stale_mirror(tmp_path):
    plan = _write_plan(tmp_path, _PLAN.replace("size: M", "size: M\nblocked_by: [x-old]"))
    node = {"blocked_by": []}

    assert project_node_to_plan(node, plan) is True
    _, fields, _ = read_plan_file(plan)
    assert fields["blocked_by"] == []


def test_missing_plan_path_warns_no_raise(tmp_path):
    node = {"priority": "p0"}
    assert project_node_to_plan(node, tmp_path / "does-not-exist.md") is False


def test_unowned_keys_untouched(tmp_path):
    plan = _write_plan(tmp_path)
    node = {"priority": "p3"}  # no _status -> status projection skipped

    project_node_to_plan(node, plan)
    _, fields, _ = read_plan_file(plan)
    # kill_criteria (RawBlock) + status/node survive the projection untouched.
    assert fields["status"] == "ready"
    assert fields["node"] == "x-abcd"
    assert "kill_criteria" in fields
    # The opaque kill_criteria block still carries its child lines verbatim.
    assert "iteration_ceiling" in plan.read_text(encoding="utf-8")


# ---------------------------------------------------------------------------
# Status projection (x-f34f)
# ---------------------------------------------------------------------------


def test_status_projects_claim_to_in_progress(tmp_path):
    """AC1-HP: a claimed node moves a ready plan to in_progress, body intact."""
    plan = _write_plan(tmp_path)
    assert project_node_to_plan({"_status": "claimed"}, plan) is True
    _, fields, _ = read_plan_file(plan)
    assert fields["status"] == "in_progress"
    assert "iteration_ceiling" in plan.read_text(encoding="utf-8")  # body untouched


def test_status_projects_done_stamps_done_at(tmp_path):
    """AC2-HP: a done node flips a shipped plan to done and stamps done_at once."""
    plan = _write_plan(tmp_path, _PLAN.replace("status: ready", "status: shipped"))
    assert project_node_to_plan({"_status": "done"}, plan) is True
    _, fields, _ = read_plan_file(plan)
    assert fields["status"] == "done"
    assert fields.get("done_at")
    first = fields["done_at"]
    # First-write-only: a second done projection never overwrites done_at.
    assert project_node_to_plan({"_status": "done"}, plan) is False
    _, fields2, _ = read_plan_file(plan)
    assert fields2["done_at"] == first


def test_status_backward_projection_refused(tmp_path):
    """AC1-EDGE: a node that regressed to claimed never rewrites a shipped plan."""
    plan = _write_plan(tmp_path, _PLAN.replace("status: ready", "status: shipped"))
    assert project_node_to_plan({"_status": "claimed"}, plan) is False
    _, fields, _ = read_plan_file(plan)
    assert fields["status"] == "shipped"


def test_status_no_write_for_gated_states(tmp_path):
    """blocked/deferred are graph-side gates: the plan status is left as-is."""
    plan = _write_plan(tmp_path)
    for gated in ("blocked", "deferred"):
        assert project_node_to_plan({"_status": gated}, plan) is False
    _, fields, _ = read_plan_file(plan)
    assert fields["status"] == "ready"
