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
    node = {"priority": "p3"}  # no status -> status projection skipped

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
    assert project_node_to_plan({"status": "claimed"}, plan) is True
    _, fields, _ = read_plan_file(plan)
    assert fields["status"] == "in_progress"
    assert "iteration_ceiling" in plan.read_text(encoding="utf-8")  # body untouched


def test_status_projects_done_stamps_done_at(tmp_path):
    """AC2-HP: a done node flips a shipped plan to done and stamps done_at once."""
    plan = _write_plan(tmp_path, _PLAN.replace("status: ready", "status: shipped"))
    assert project_node_to_plan({"status": "done"}, plan) is True
    _, fields, _ = read_plan_file(plan)
    assert fields["status"] == "done"
    assert fields.get("done_at")
    first = fields["done_at"]
    # First-write-only: a second done projection never overwrites done_at.
    assert project_node_to_plan({"status": "done"}, plan) is False
    _, fields2, _ = read_plan_file(plan)
    assert fields2["done_at"] == first


def test_status_backward_projection_refused(tmp_path):
    """AC1-EDGE: a node that regressed to claimed never rewrites a shipped plan."""
    plan = _write_plan(tmp_path, _PLAN.replace("status: ready", "status: shipped"))
    assert project_node_to_plan({"status": "claimed"}, plan) is False
    _, fields, _ = read_plan_file(plan)
    assert fields["status"] == "shipped"


def test_status_no_write_for_gated_states(tmp_path):
    """blocked/deferred are graph-side gates: the plan status is left as-is."""
    plan = _write_plan(tmp_path)
    for gated in ("blocked", "deferred"):
        assert project_node_to_plan({"status": gated}, plan) is False
    _, fields, _ = read_plan_file(plan)
    assert fields["status"] == "ready"


# ---------------------------------------------------------------------------
# Extended mirror keys: size, parent, parent_slug (x-5d84)
# ---------------------------------------------------------------------------


def test_projects_size_and_parent(tmp_path):
    """AC3-HP: size + parent + parent_slug mirror when present on the node."""
    plan = _write_plan(tmp_path, _PLAN.replace("size: M", "size: S"))
    node = {"size": "M", "parent": "x-fd7f", "parent_slug": "epic-slug"}

    assert project_node_to_plan(node, plan) is True
    _, fields, _ = read_plan_file(plan)
    assert fields["size"] == "M"
    assert fields["parent"] == "x-fd7f"
    assert fields["parent_slug"] == "epic-slug"


def test_null_parent_writes_neither_key(tmp_path):
    """A top-level node (parent None, parent_slug absent) writes neither key."""
    plan = _write_plan(tmp_path)
    node = {"parent": None, "size": "L"}

    assert project_node_to_plan(node, plan) is True  # size changed
    _, fields, _ = read_plan_file(plan)
    assert "parent" not in fields
    assert "parent_slug" not in fields
    assert fields["size"] == "L"


def test_cleared_nullable_mirror_removes_stale_frontmatter(tmp_path):
    """A de-orphan / --size null clears the stale doc mirror, not leaves it."""
    seeded = _PLAN.replace(
        "size: M", "size: M\nparent: x-old\nparent_slug: old-epic"
    )
    plan = _write_plan(tmp_path, seeded)
    # Graph dropped parent (and its slug) and cleared size to None.
    node = {"parent": None, "parent_slug": None, "size": None}

    assert project_node_to_plan(node, plan) is True
    _, fields, _ = read_plan_file(plan)
    assert "parent" not in fields
    assert "parent_slug" not in fields
    assert "size" not in fields


def test_non_clearable_none_never_deletes(tmp_path):
    """A None on a non-clearable key (priority) never deletes a real doc value."""
    plan = _write_plan(tmp_path, _PLAN.replace("type: feature", "type: feature\npriority: p1"))
    node = {"priority": None}

    assert project_node_to_plan(node, plan) is False
    _, fields, _ = read_plan_file(plan)
    assert fields["priority"] == "p1"  # untouched


# ---------------------------------------------------------------------------
# tags mirror (x-6c2b wave 1) - same list semantics as blocked_by
# ---------------------------------------------------------------------------


def test_tags_mirror_reaches_doc(tmp_path):
    """AC1: a node's tags list mirrors into the plan frontmatter."""
    plan = _write_plan(tmp_path)
    node = {"tags": ["mux"]}

    assert project_node_to_plan(node, plan) is True
    _, fields, _ = read_plan_file(plan)
    assert fields["tags"] == ["mux"]


def test_empty_tags_clears_stale_mirror(tmp_path):
    """An empty tags list clears a stale doc mirror (like blocked_by)."""
    plan = _write_plan(tmp_path, _PLAN.replace("size: M", "size: M\ntags: [old]"))
    node = {"tags": []}

    assert project_node_to_plan(node, plan) is True
    _, fields, _ = read_plan_file(plan)
    assert fields["tags"] == []
