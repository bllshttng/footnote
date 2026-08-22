"""Integration tests for graph collision detection.

Covers:
- ``parse_files_to_modify`` for quick plans
- ``find_collisions`` severity scoring + action inference
- ``_load_thresholds`` layered config
- ``fno backlog collisions check`` CLI verb
- ``fno backlog supersede`` CLI verb
- ``cmd_update --acknowledge-collisions`` audit-trail field
- ``find_acknowledged_collisions`` resolved-collision reconciliation
- ``fno backlog triage health`` aggregate report
- ``superseded`` ``status`` derivation
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    """Fresh empty graph.json routed to tmp_path.

    Mirrors the fixture pattern used by test_defer.py so all the constants
    that point at ``~/.fno/graph.json`` redirect into the test sandbox.
    """
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    # Seam readers resolve fno.paths.graph_json at call time; pin the
    # resolver to the same hermetic file (module-attr pins do not reach it).
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


def _invoke(*args, input=None):
    return runner.invoke(app, list(args), input=input, catch_exceptions=False)


def _read_entries(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


def _plan_status(path: Path):
    """Read the `status` field from a plan doc's frontmatter, or None."""
    from fno.plan._stamp import read_plan_file

    try:
        _target, fields, _rest = read_plan_file(path)
    except Exception:
        return None
    return fields.get("status")


def _set_plan_status(path: Path, status: str) -> None:
    from fno.plan._stamp import read_plan_file, write_plan_file

    target, fields, rest = read_plan_file(path)
    fields["status"] = status
    write_plan_file(target, fields, rest)


def _write_quick_plan(path: Path, files: list[str], title: str = "Test plan") -> Path:
    """Write a single-file quick plan with a Files to Modify table."""
    rows = "\n".join(f"| `{f}` | edit |" for f in files)
    path.write_text(
        f"""---
created: 2026-04-27
scope: feature
domain: code
---

# {title}

## Context

Some context.

## Files to Modify

| File | Action |
|------|--------|
{rows}
"""
    )
    return path


# ---------------------------------------------------------------------------
# parse_files_to_modify
# ---------------------------------------------------------------------------


def test_parse_files_quick_plan(tmp_path):
    from fno.graph.collision import parse_files_to_modify

    plan = _write_quick_plan(tmp_path / "p.md", ["src/a.py", "src/b.py", "src/c.py"])
    out = parse_files_to_modify(plan)
    assert out == {"src/a.py", "src/b.py", "src/c.py"}


def test_parse_strips_parentheticals_and_line_suffixes(tmp_path):
    from fno.graph.collision import parse_files_to_modify

    plan = tmp_path / "p.md"
    plan.write_text(
        """# t

## Files to Modify

| File | Action |
|------|--------|
| `path/to/file.py:42` | edit |
| `~/.fno/settings.yaml` (template) | edit |
| `cli/src/fno/setup/` | edit |
"""
    )
    out = parse_files_to_modify(plan)
    assert "path/to/file.py" in out
    assert "~/.fno/settings.yaml" in out
    assert "cli/src/fno/setup/" in out
    # Line suffix should be stripped, not preserved
    assert "path/to/file.py:42" not in out


# ---------------------------------------------------------------------------
# find_collisions
# ---------------------------------------------------------------------------


def _seed_node(graph: list[dict], *, id_: str, plan_path: str, created_at: str = "2026-04-27T00:00:00+00:00") -> dict:
    node = {
        "id": id_,
        "parent": None,
        "title": f"Node {id_}",
        "type": "feature",
        "project": "fno",
        "cwd": "/repo",
        "priority": "p2",
        "domain": "code",
        "blocked_by": [],
        "session_id": None,
        "claimed_at": None,
        "completed_at": None,
        "status": "ready",
        "has_brief": False,
        "compacted": False,
        "plan_path": plan_path,
        "pr_number": None,
        "pr_url": None,
        "merge_status": None,
        "created_at": created_at,
    }
    graph.append(node)
    return node


def test_no_collision_when_files_disjoint(tmp_path):
    from fno.graph.collision import find_collisions

    a = _write_quick_plan(tmp_path / "a.md", ["src/a.py"])
    b = _write_quick_plan(tmp_path / "b.md", ["src/b.py"])
    graph: list[dict] = []
    _seed_node(graph, id_="ab-other", plan_path=str(b))

    cols = find_collisions(a, graph)
    assert cols == []


def test_high_severity_when_subset(tmp_path):
    from fno.graph.collision import find_collisions

    # candidate is a strict subset of other -> absorb
    cand = _write_quick_plan(tmp_path / "cand.md", ["src/a.py", "src/b.py"])
    other = _write_quick_plan(tmp_path / "other.md", ["src/a.py", "src/b.py", "src/c.py", "src/d.py"])
    graph: list[dict] = []
    _seed_node(graph, id_="ab-other", plan_path=str(other))

    cols = find_collisions(cand, graph)
    assert len(cols) == 1
    assert cols[0].severity == "high"
    assert cols[0].recommended_action == "absorb"


def test_high_severity_when_superset(tmp_path):
    from fno.graph.collision import find_collisions

    # candidate is a strict superset of other -> supersede
    cand = _write_quick_plan(tmp_path / "cand.md", ["src/a.py", "src/b.py", "src/c.py", "src/d.py"])
    other = _write_quick_plan(tmp_path / "other.md", ["src/a.py", "src/b.py"])
    graph: list[dict] = []
    _seed_node(graph, id_="ab-other", plan_path=str(other))

    cols = find_collisions(cand, graph)
    assert len(cols) == 1
    assert cols[0].severity == "high"
    assert cols[0].recommended_action == "supersede"


def test_medium_severity_partial_overlap(tmp_path):
    from fno.graph.collision import find_collisions

    cand = _write_quick_plan(tmp_path / "cand.md", ["src/a.py", "src/b.py", "src/c.py", "src/d.py"])
    other = _write_quick_plan(tmp_path / "other.md", ["src/a.py", "src/b.py", "src/x.py", "src/y.py"])
    graph: list[dict] = []
    _seed_node(graph, id_="ab-other", plan_path=str(other))

    cols = find_collisions(cand, graph)
    assert len(cols) == 1
    # 2 shared of 4 each: ratio = 0.5 of min; that hits high_ratio (0.5)
    # Default thresholds => high. Test expects medium thresholds when ratio
    # tweaked OR explicit medium signal. Override thresholds to verify
    # the medium classification path under stricter defaults.
    cols_strict = find_collisions(
        cand, graph,
        thresholds={"high_count": 5, "high_ratio": 0.9, "medium_count": 2, "medium_ratio": 0.25},
    )
    assert cols_strict[0].severity == "medium"


def test_low_severity_single_overlap(tmp_path):
    from fno.graph.collision import find_collisions

    cand = _write_quick_plan(tmp_path / "cand.md", ["src/a.py", "src/b.py", "src/c.py", "src/d.py", "src/e.py"])
    other = _write_quick_plan(tmp_path / "other.md", ["src/a.py", "src/x.py", "src/y.py", "src/z.py", "src/w.py"])
    graph: list[dict] = []
    _seed_node(graph, id_="ab-other", plan_path=str(other))

    cols = find_collisions(
        cand, graph,
        thresholds={"high_count": 3, "high_ratio": 0.5, "medium_count": 2, "medium_ratio": 0.25},
    )
    assert len(cols) == 1
    assert cols[0].severity == "low"
    # Low severity gets a coordinate recommendation with split rationale
    assert cols[0].recommended_action == "coordinate"
    assert "split" in cols[0].rationale.lower()


def test_self_collision_excluded(tmp_path):
    from fno.graph.collision import find_collisions

    cand = _write_quick_plan(tmp_path / "cand.md", ["src/a.py", "src/b.py"])
    other = _write_quick_plan(tmp_path / "other.md", ["src/a.py", "src/b.py"])
    graph: list[dict] = []
    _seed_node(graph, id_="ab-self", plan_path=str(cand))
    _seed_node(graph, id_="ab-other", plan_path=str(other))

    cols = find_collisions(cand, graph, self_id="ab-self")
    assert len(cols) == 1
    assert cols[0].with_node_id == "ab-other"


def test_done_nodes_excluded(tmp_path):
    """Done plans should not show as collisions; the work already shipped."""
    from fno.graph.collision import find_collisions

    cand = _write_quick_plan(tmp_path / "cand.md", ["src/a.py", "src/b.py"])
    other = _write_quick_plan(tmp_path / "other.md", ["src/a.py", "src/b.py"])
    graph: list[dict] = []
    _seed_node(graph, id_="ab-done", plan_path=str(other))
    graph[0]["completed_at"] = "2026-04-27T00:00:00+00:00"
    graph[0]["status"] = "done"

    cols = find_collisions(cand, graph)
    assert cols == []


# ---------------------------------------------------------------------------
# _load_thresholds
# ---------------------------------------------------------------------------


def test_load_thresholds_defaults(tmp_path):
    from fno.graph.collision import _load_thresholds, DEFAULT_THRESHOLDS

    # Point both layers at non-existent paths.
    out = _load_thresholds(
        project_settings=tmp_path / "missing-project.yaml",
        user_settings=tmp_path / "missing-user.yaml",
    )
    assert out == DEFAULT_THRESHOLDS


def test_load_thresholds_project_beats_user(tmp_path):
    from fno.graph.collision import _load_thresholds

    user = tmp_path / "user.yaml"
    user.write_text(
        """config:
  collision:
    severity_thresholds:
      high_count: 5
      high_ratio: 0.9
      medium_count: 4
      medium_ratio: 0.4
"""
    )
    project = tmp_path / "project.yaml"
    project.write_text(
        """config:
  collision:
    severity_thresholds:
      high_count: 2
"""
    )
    out = _load_thresholds(project_settings=project, user_settings=user)
    # project wins where present
    assert out["high_count"] == 2
    # user fills in the rest
    assert out["high_ratio"] == 0.9
    assert out["medium_count"] == 4
    assert out["medium_ratio"] == 0.4


def test_load_thresholds_malformed_warns_and_falls_back(tmp_path, caplog):
    import logging

    from fno.graph.collision import _load_thresholds, DEFAULT_THRESHOLDS

    proj = tmp_path / "project.yaml"
    proj.write_text(
        """config:
  collision:
    severity_thresholds:
      high_count: not-a-number
      high_ratio: -0.5
"""
    )
    with caplog.at_level(logging.WARNING, logger="fno.config"):
        out = _load_thresholds(project_settings=proj, user_settings=tmp_path / "missing.yaml")
    # The model's per-key sanitizer warns via the logger and falls back to the
    # modeled default for the bad key.
    assert any(
        "not numeric" in r.message or "negative" in r.message for r in caplog.records
    )
    assert out["high_count"] == DEFAULT_THRESHOLDS["high_count"]
    assert out["high_ratio"] == DEFAULT_THRESHOLDS["high_ratio"]


# ---------------------------------------------------------------------------
# _resolve_plan_path - covers the three shapes seen on the live graph
# ---------------------------------------------------------------------------


def test_resolve_plan_path_absolute(tmp_path):
    from fno.graph.collision import _resolve_plan_path

    abs_path = str(tmp_path / "plan.md")
    repo = tmp_path / "repo"
    out = _resolve_plan_path(abs_path, repo)
    assert out == Path(abs_path)


def test_resolve_plan_path_tilde_expanded(tmp_path):
    from fno.graph.collision import _resolve_plan_path

    out = _resolve_plan_path("~/foo/bar.md", tmp_path)
    # Tilde must be expanded; result is absolute and points outside repo_root
    assert out.is_absolute()
    assert "foo/bar.md" in str(out)
    assert "~" not in str(out)


def test_resolve_plan_path_repo_relative(tmp_path):
    from fno.graph.collision import _resolve_plan_path

    out = _resolve_plan_path("internal/plans/x.md", tmp_path / "repo")
    assert out == tmp_path / "repo" / "internal" / "plans" / "x.md"


# ---------------------------------------------------------------------------
# CLI: collisions check
# ---------------------------------------------------------------------------


def test_cli_collisions_check_no_collisions(tmp_graph, tmp_path):
    plan = _write_quick_plan(tmp_path / "lonely.md", ["src/lonely.py"])
    res = _invoke("backlog", "collisions", "check", str(plan))
    assert res.exit_code == 0, res.output
    assert "No collisions found" in res.output


def test_cli_collisions_check_emits_json(tmp_graph, tmp_path):
    other = _write_quick_plan(tmp_path / "other.md", ["src/a.py", "src/b.py"])
    cand = _write_quick_plan(tmp_path / "cand.md", ["src/a.py", "src/b.py"])

    # Adopt the other plan as a node
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-other", plan_path=str(other))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "collisions", "check", str(cand), "--json")
    assert res.exit_code == 0, res.output
    payload = json.loads(res.output)
    assert payload["status"] == "ok"
    found = payload["collisions"]
    assert len(found) == 1
    assert found[0]["with_node_id"] == "ab-other"
    assert "_other_created_at" not in found[0]


# ---------------------------------------------------------------------------
# CLI: supersede
# ---------------------------------------------------------------------------


def test_supersede_writes_both_directions(tmp_graph, tmp_path):
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["x.py", "y.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-old", "--cause", "consolidated", "--surface", "x.py")
    assert res.exit_code == 0, res.output

    entries = _read_entries(tmp_graph)
    by_id = {e["id"]: e for e in entries}
    assert by_id["ab-old"]["superseded_by"] == "ab-new"
    assert "ab-old" in by_id["ab-new"]["supersedes"]


def test_supersede_requires_cause_and_surface_before_mutation(tmp_graph, tmp_path):
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["x.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    result = _invoke(
        "backlog", "supersede", "ab-new", "--replaces", "ab-old",
        "--reason", "consolidated",
    )

    assert result.exit_code != 0
    assert "cause" in result.output.lower()
    assert {e["id"]: e for e in _read_entries(tmp_graph)}["ab-old"].get(
        "superseded_by"
    ) is None


def test_supersede_records_pending_evidence_without_terminalizing(tmp_graph, tmp_path):
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["src/old.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["src/new.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    result = _invoke(
        "backlog", "supersede", "ab-new", "--replaces", "ab-old",
        "--cause", "old implementation replaced", "--surface", "src/old.py",
    )

    assert result.exit_code == 0, result.output
    old = {e["id"]: e for e in _read_entries(tmp_graph)}["ab-old"]
    assert old["superseded_by"] == "ab-new"
    assert old["supersession"] == {
        "successor": "ab-new",
        "cause": "old implementation replaced",
        # No --reason was passed, so the slot is present and empty rather than
        # absent: readers get one shape either way.
        "reason": None,
        "surfaces": ["src/old.py"],
        "verified_at": None,
        "evidence_pr": None,
        "matched_surfaces": [],
    }
    assert old.get("deferred_at") is None
    assert old["status"] != "superseded"


def test_supersede_keeps_old_pending_until_verified(tmp_graph, tmp_path):
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["x.py", "y.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-old", "--cause", "consolidated", "--surface", "x.py")
    assert res.exit_code == 0, res.output

    entries = _read_entries(tmp_graph)
    by_id = {e["id"]: e for e in entries}
    assert by_id["ab-old"]["deferred_at"] is None
    assert by_id["ab-old"]["status"] == "blocked"
    assert "pending supersession" in by_id["ab-old"]["blocked_reason"]


def test_supersede_done_node_rejected(tmp_graph, tmp_path):
    """Superseding a shipped node would erase its completed_at and destroy
    forensic history. Refuse the mutation; user opens a follow-up instead."""
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-shipped", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["y.py"])))
    entries[0]["completed_at"] = "2026-04-30T12:00:00+00:00"
    entries[0]["status"] = "done"
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-shipped", "--cause", "test", "--surface", "x.py")
    assert res.exit_code != 0
    assert "already shipped" in res.output.lower() or "status=done" in res.output

    # Confirm no mutation happened: completed_at preserved, no superseded_by written
    entries = _read_entries(tmp_graph)
    by_id = {e["id"]: e for e in entries}
    assert by_id["ab-shipped"]["completed_at"] == "2026-04-30T12:00:00+00:00"
    assert by_id["ab-shipped"].get("superseded_by") is None


def test_supersede_already_superseded_rejected(tmp_graph, tmp_path):
    """Chaining supersede on an already-superseded node would corrupt the
    chain. Refuse and ask the user to resolve the existing chain."""
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(entries, id_="ab-mid", plan_path=str(_write_quick_plan(tmp_path / "mid.md", ["y.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["z.py"])))
    entries[0]["superseded_by"] = "ab-mid"
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-old", "--cause", "test", "--surface", "x.py")
    assert res.exit_code != 0
    assert "already superseded" in res.output.lower()


def test_supersede_self_rejected(tmp_graph, tmp_path):
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-x", plan_path=str(_write_quick_plan(tmp_path / "x.md", ["x.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "supersede", "ab-x", "--replaces", "ab-x", "--cause", "test", "--surface", "x.py")
    assert res.exit_code != 0
    assert "supersede self" in res.output


def test_supersede_blank_reason_rejected(tmp_graph, tmp_path):
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["y.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-old", "--cause", "   ", "--surface", "x.py")
    assert res.exit_code != 0
    assert "blank" in res.output.lower()


def test_supersede_deferred_node_rejected(tmp_graph, tmp_path):
    """A deferred node carries an independent park. Superseding would overwrite
    it and unsupersede could not restore it, so refuse - same shape as the
    done and already-superseded refusals."""
    entries = _read_entries(tmp_graph)
    old = _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    old["deferred_at"] = "2026-07-01T00:00:00+00:00"
    old["deferred_reason"] = "parked"
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["y.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-old", "--cause", "fold", "--surface", "x.py")
    assert res.exit_code != 0
    assert "deferred" in res.output.lower()
    # No mutation; the park is preserved.
    by_id = {e["id"]: e for e in _read_entries(tmp_graph)}
    assert by_id["ab-old"]["deferred_at"] == "2026-07-01T00:00:00+00:00"
    assert by_id["ab-old"].get("superseded_by") is None


def test_supersede_live_children_rejected(tmp_graph, tmp_path):
    """Superseding a unit with live children strands them under a dead unit
    (dead-ancestor guard + inherited priority). Refuse, name the children,
    and leave the graph untouched. Gates on liveness, not type."""
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["y.py"])))
    for kid in ("ab-k1", "ab-k2"):
        node = _seed_node(entries, id_=kid, plan_path=str(_write_quick_plan(tmp_path / f"{kid}.md", ["z.py"])))
        node["parent"] = "ab-old"
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-old", "--cause", "fold", "--surface", "x.py")
    assert res.exit_code != 0
    assert "live child" in res.output.lower()
    assert "ab-k1" in res.output and "ab-k2" in res.output
    assert "--force" in res.output

    # No mutation happened.
    entries = _read_entries(tmp_graph)
    by_id = {e["id"]: e for e in entries}
    assert by_id["ab-old"].get("superseded_by") is None
    assert by_id["ab-k1"]["parent"] == "ab-old"


def test_supersede_force_orphans_live_children(tmp_graph, tmp_path):
    """--force overrides the guard; live children have parent cleared so they
    stay dispatchable instead of stranding under the dead unit."""
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["y.py"])))
    node = _seed_node(entries, id_="ab-k1", plan_path=str(_write_quick_plan(tmp_path / "k1.md", ["z.py"])))
    node["parent"] = "ab-old"
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke(
        "backlog", "supersede", "ab-new", "--replaces", "ab-old", "--cause", "fold", "--surface", "x.py", "--force"
    )
    assert res.exit_code == 0, res.output
    assert "Cleared parent" in res.output and "ab-k1" in res.output

    entries = _read_entries(tmp_graph)
    by_id = {e["id"]: e for e in entries}
    assert by_id["ab-old"]["superseded_by"] == "ab-new"
    assert by_id["ab-k1"]["parent"] is None


def test_supersede_done_child_kept_revivable_released(tmp_graph, tmp_path):
    """A unit with no LIVE children can be superseded without --force. A done
    child keeps parent as history; a deferred child is released (parent cleared)
    so a later undefer cannot strand it under the dead unit."""
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["y.py"])))
    done = _seed_node(entries, id_="ab-done", plan_path=str(_write_quick_plan(tmp_path / "done.md", ["z.py"])))
    done["parent"] = "ab-old"
    done["completed_at"] = "2026-07-01T00:00:00+00:00"
    done["status"] = "done"
    deferred = _seed_node(entries, id_="ab-def", plan_path=str(_write_quick_plan(tmp_path / "def.md", ["z.py"])))
    deferred["parent"] = "ab-old"
    deferred["deferred_at"] = "2026-07-01T00:00:00+00:00"
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-old", "--cause", "fold", "--surface", "x.py")
    assert res.exit_code == 0, res.output

    entries = _read_entries(tmp_graph)
    by_id = {e["id"]: e for e in entries}
    assert by_id["ab-old"]["superseded_by"] == "ab-new"
    # Done keeps parent (history); deferred is released so undefer cannot strand.
    assert by_id["ab-done"]["parent"] == "ab-old"
    assert by_id["ab-def"]["parent"] is None


def test_unsupersede_restores_node_and_clears_backref(tmp_graph, tmp_path):
    """unsupersede is the reverse cmd_supersede always needed: clears
    superseded_by + the deferred markers it set, drops the backref on the
    replacer, and lets status recompute off the superseded bucket."""
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["y.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-old", "--cause", "fold", "--surface", "x.py")
    assert res.exit_code == 0, res.output
    assert {e["id"]: e for e in _read_entries(tmp_graph)}["ab-old"]["status"] == "blocked"

    res = _invoke("backlog", "unsupersede", "ab-old")
    assert res.exit_code == 0, res.output

    by_id = {e["id"]: e for e in _read_entries(tmp_graph)}
    assert by_id["ab-old"].get("superseded_by") is None
    assert by_id["ab-old"].get("deferred_at") is None
    assert "ab-old" not in (by_id["ab-new"].get("supersedes") or [])
    assert by_id["ab-old"]["status"] != "superseded"


def test_unsupersede_resets_plan_status_off_terminal(tmp_graph, tmp_path):
    """supersede stamps `status: superseded` into the plan; the forward-only
    projector refuses to leave that terminal, so unsupersede must force the
    plan back in step with the graph or plan consumers stay inconsistent."""
    plan = _write_quick_plan(tmp_path / "old.md", ["x.py"])
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-old", plan_path=str(plan))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["y.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-old", "--cause", "fold", "--surface", "x.py")
    assert _plan_status(plan) is None

    res = _invoke("backlog", "unsupersede", "ab-old")
    assert res.exit_code == 0, res.output
    assert _plan_status(plan) == "ready"


def test_unsupersede_preserves_plain_deferral(tmp_graph, tmp_path):
    """unsupersede on a node that is merely deferred (not superseded) must NOT
    clear the deferral - reactivating parked work is undefer's job."""
    entries = _read_entries(tmp_graph)
    node = _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    node["deferred_at"] = "2026-07-01T00:00:00+00:00"
    node["deferred_reason"] = "parked"
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "unsupersede", "ab-old")
    assert res.exit_code == 0
    assert "was not superseded" in res.output
    by_id = {e["id"]: e for e in _read_entries(tmp_graph)}
    # Deferral untouched.
    assert by_id["ab-old"]["deferred_at"] == "2026-07-01T00:00:00+00:00"
    assert by_id["ab-old"]["deferred_reason"] == "parked"


def test_unsupersede_blocked_plan_fails_closed_to_design(tmp_graph, tmp_path):
    """A revived node that is still blocked has no plan rung to restore to (the
    prior rung was overwritten by supersede). Fail closed to a non-dispatchable
    rung rather than `ready`, which would auto-dispatch unfinished planning work
    the moment its blocker resolves."""
    plan = _write_quick_plan(tmp_path / "old.md", ["x.py"])
    entries = _read_entries(tmp_graph)
    old = _seed_node(entries, id_="ab-old", plan_path=str(plan))
    old["blocked_by"] = ["ab-blk"]
    _seed_node(entries, id_="ab-blk", plan_path=str(_write_quick_plan(tmp_path / "blk.md", ["z.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["y.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-old", "--cause", "fold", "--surface", "x.py")
    assert _plan_status(plan) is None

    res = _invoke("backlog", "unsupersede", "ab-old")
    assert res.exit_code == 0, res.output
    assert _plan_status(plan) is None


def test_force_supersede_does_not_corrupt_shared_plan(tmp_graph, tmp_path):
    """Adopted children share the delivery unit's plan_path. Force-supersede
    must not project each such child: that would rewrite the one shared doc per
    child and the last pass would overwrite the owner's mirrored metadata."""
    shared = _write_quick_plan(tmp_path / "shared.md", ["x.py"])
    entries = _read_entries(tmp_graph)
    owner = _seed_node(entries, id_="ab-old", plan_path=str(shared))
    owner["priority"] = "p0"
    for kid in ("ab-c1", "ab-c2"):
        adopted = _seed_node(entries, id_=kid, plan_path=str(shared))
        adopted["contained_in"] = "ab-old"
        adopted["parent"] = "ab-old"
        adopted["priority"] = "p3"
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["y.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke(
        "backlog", "supersede", "ab-new", "--replaces", "ab-old", "--cause", "fold", "--surface", "x.py", "--force"
    )
    assert res.exit_code == 0, res.output
    # The shared plan keeps the OWNER's priority, not a child's p3.
    from fno.plan._stamp import read_plan_file
    _t, fields, _r = read_plan_file(shared)
    assert fields.get("priority") == "p0", f"shared plan corrupted to {fields.get('priority')!r}"


def test_supersede_guard_not_bypassed_by_abbreviated_id(tmp_graph, tmp_path):
    """The guard must compare against the canonical resolved id, not the raw
    --replaces argument: an abbreviated id resolves via _find_node but never
    equals a child's full canonical parent, which would silently bypass it."""
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-aabbccdd", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(entries, id_="ab-eeffffff", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["y.py"])))
    kid = _seed_node(entries, id_="ab-11223344", plan_path=str(_write_quick_plan(tmp_path / "kid.md", ["z.py"])))
    kid["parent"] = "ab-aabbccdd"
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    # Abbreviated --replaces (resolves to ab-aabbccdd); the live child must
    # still trip the guard.
    res = _invoke("backlog", "supersede", "ab-eeffffff", "--replaces", "ab-aabbcc", "--cause", "fold", "--surface", "x.py")
    assert res.exit_code != 0
    assert "live child" in res.output.lower()
    assert "ab-11223344" in res.output


def test_unsupersede_preserves_done_plan(tmp_graph, tmp_path):
    """A node with both completed_at and superseded_by (e.g. `done --force` on
    a superseded advisory) stays done through unsupersede: done is read from
    completed_at, not the stale plan rung, so it must not regress to design."""
    plan = _write_quick_plan(tmp_path / "old.md", ["x.py"])
    entries = _read_entries(tmp_graph)
    node = _seed_node(entries, id_="ab-old", plan_path=str(plan))
    node["completed_at"] = "2026-07-01T00:00:00+00:00"
    node["superseded_by"] = "ab-new"
    node["status"] = "done"
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["y.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))
    _set_plan_status(plan, "superseded")

    res = _invoke("backlog", "unsupersede", "ab-old")
    assert res.exit_code == 0, res.output
    assert _plan_status(plan) == "done"
    # The force path stamps done_at just like a normal done promotion.
    from fno.plan._stamp import read_plan_file
    _t, fields, _r = read_plan_file(plan)
    assert fields.get("done_at"), "done promotion must carry done_at"


def test_unsupersede_not_superseded_is_idempotent(tmp_graph, tmp_path):
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "unsupersede", "ab-old")
    assert res.exit_code == 0
    assert "was not superseded" in res.output


# ---------------------------------------------------------------------------
# CLI: update --acknowledge-collisions
# ---------------------------------------------------------------------------


def test_acknowledge_collisions_writes_audit_field(tmp_graph, tmp_path):
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["x.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke(
        "backlog", "update", "ab-new",
        "--acknowledge-collisions", "ab-old1,ab-old2",
    )
    assert res.exit_code == 0, res.output
    entries = _read_entries(tmp_graph)
    by_id = {e["id"]: e for e in entries}
    assert by_id["ab-new"]["collisions_acknowledged"] == ["ab-old1", "ab-old2"]


def test_acknowledge_collisions_skipped_sentinel(tmp_graph, tmp_path):
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["x.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke(
        "backlog", "update", "ab-new",
        "--acknowledge-collisions", "__skipped_check__",
    )
    assert res.exit_code == 0, res.output
    entries = _read_entries(tmp_graph)
    by_id = {e["id"]: e for e in entries}
    assert by_id["ab-new"]["collisions_acknowledged"] == ["__skipped_check__"]


# ---------------------------------------------------------------------------
# find_acknowledged_collisions reconciliation
# ---------------------------------------------------------------------------


def test_acknowledged_resolved_when_other_ships(tmp_path):
    from fno.graph.collision import find_acknowledged_collisions

    graph: list[dict] = []
    _seed_node(graph, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(graph, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["x.py"])))
    # ab-new acknowledged a collision with ab-old; ab-old then shipped.
    graph[0]["completed_at"] = "2026-04-28T00:00:00+00:00"
    graph[0]["status"] = "done"
    graph[1]["collisions_acknowledged"] = ["ab-old"]

    out = find_acknowledged_collisions(graph)
    assert len(out) == 1
    assert out[0].node_id == "ab-new"
    assert out[0].resolved_via == "ab-old"
    assert out[0].resolved_via_status == "done"


def test_acknowledged_skipped_sentinel_ignored(tmp_path):
    from fno.graph.collision import find_acknowledged_collisions

    graph: list[dict] = []
    _seed_node(graph, id_="ab-x", plan_path=str(_write_quick_plan(tmp_path / "x.md", ["x.py"])))
    graph[0]["collisions_acknowledged"] = ["__skipped_check__"]

    out = find_acknowledged_collisions(graph)
    assert out == []


# ---------------------------------------------------------------------------
# CLI: triage health
# ---------------------------------------------------------------------------


def test_triage_health_reports_collisions(tmp_graph, tmp_path):
    """Two pending plans touching the same files surface as a collision."""
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-a", plan_path=str(_write_quick_plan(tmp_path / "a.md", ["src/a.py", "src/b.py"])))
    _seed_node(entries, id_="ab-b", plan_path=str(_write_quick_plan(tmp_path / "b.md", ["src/a.py", "src/b.py", "src/c.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "triage", "health", "--all", "--json")
    assert res.exit_code == 0, res.output
    report = json.loads(res.output)
    assert report["totals"]["collisions"] >= 1
    pair = sorted(report["collisions"][0]["between"])
    assert pair == ["ab-a", "ab-b"]


def test_triage_health_idea_count(tmp_graph, tmp_path):
    """Plan-less nodes count toward idea_pile_depth."""
    entries = _read_entries(tmp_graph)
    # No plan_path means idea
    _seed_node(entries, id_="ab-idea1", plan_path=None)
    _seed_node(entries, id_="ab-idea2", plan_path=None)
    _seed_node(entries, id_="ab-real", plan_path=str(_write_quick_plan(tmp_path / "r.md", ["x.py"])))
    # Force the idea state: clear plan_path post-seed
    for e in entries:
        if e["id"] in ("ab-idea1", "ab-idea2"):
            e["plan_path"] = None
            e["status"] = "idea"
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "triage", "health", "--all", "--json")
    assert res.exit_code == 0, res.output
    report = json.loads(res.output)
    assert report["idea_pile_depth"] == 2


def test_triage_health_failure_prone(tmp_graph, tmp_path):
    """Multi-attempt nodes with no PR show as failure-prone."""
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-burn", plan_path=str(_write_quick_plan(tmp_path / "p.md", ["x.py"])))
    entries[-1]["cost_sessions"] = [
        {"cost_usd": 5.0},
        {"cost_usd": 8.0},
    ]
    entries[-1]["pr_number"] = None
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "triage", "health", "--all", "--json")
    assert res.exit_code == 0, res.output
    report = json.loads(res.output)
    assert any(n["id"] == "ab-burn" for n in report["failure_prone_nodes"])
    assert report["failure_prone_nodes"][0]["burned_usd"] == 13.0


def test_triage_health_shows_evals_line_when_history_exists(tmp_graph, tmp_path, monkeypatch):
    """The evals consumer: triage health surfaces regression rate + flakes when
    eval history exists (US4). A regression-tier task with a failure flags the
    alarm; evals is advisory and never changes the health exit code."""
    import fno.paths as _paths
    from fno.evals import history as _eh

    hist = tmp_path / "evals-history.jsonl"
    _eh.append_row(hist, {"task_id": "r", "tier": "regression", "pass": True})
    _eh.append_row(hist, {"task_id": "r", "tier": "regression", "pass": False})
    monkeypatch.setattr(_paths, "evals_history", lambda: hist)

    res = _invoke("backlog", "triage", "health", "--all", "--json")
    assert res.exit_code == 0, res.output
    report = json.loads(res.output)
    assert "evals" in report
    assert report["evals"]["flake_count"] == 1
    assert report["evals"]["regression_alarm"] == ["r"]


def test_triage_health_no_evals_line_without_history(tmp_graph, tmp_path, monkeypatch):
    """No history -> no evals key (line shows only when there is data)."""
    import fno.paths as _paths

    monkeypatch.setattr(_paths, "evals_history", lambda: tmp_path / "absent.jsonl")
    res = _invoke("backlog", "triage", "health", "--all", "--json")
    assert res.exit_code == 0, res.output
    assert "evals" not in json.loads(res.output)


def test_triage_health_resolves_relative_plan_paths(tmp_graph, tmp_path, monkeypatch):
    """Per gemini PR #189 review: when graph entries store repo-relative
    plan_paths and triage health is invoked from a non-repo-root cwd, the
    candidate path must be resolved against the repo root before being
    passed into find_collisions, otherwise the all-pairs loop silently
    yields zero collisions (false negatives).

    Simulates the scenario by using relative plan_paths in the graph and
    monkey-patching _find_repo_root to a known root so the test does not
    depend on the test runner's actual cwd.
    """
    import fno.graph.collision as collision

    # Create plans inside a fake repo root.
    repo = tmp_path / "fakerepo"
    repo.mkdir()
    (repo / "plans").mkdir()
    plan_a = repo / "plans" / "a.md"
    plan_b = repo / "plans" / "b.md"
    _write_quick_plan(plan_a, ["src/a.py", "src/b.py", "src/c.py"])
    _write_quick_plan(plan_b, ["src/a.py", "src/b.py", "src/c.py"])

    # Pin the resolver to our fake repo, regardless of where pytest runs.
    monkeypatch.setattr(collision, "_repo_root_cache", None)
    monkeypatch.setattr(collision, "_find_repo_root", lambda: repo)

    entries = _read_entries(tmp_graph)
    # Store relative plan_paths the way intake does on the live graph.
    _seed_node(entries, id_="ab-rel1", plan_path="plans/a.md")
    _seed_node(entries, id_="ab-rel2", plan_path="plans/b.md")
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "triage", "health", "--all", "--json")
    assert res.exit_code == 0, res.output
    report = json.loads(res.output)
    # Without the fix, collisions == 0 (false negative).
    assert report["totals"]["collisions"] >= 1, (
        f"expected at least one collision pair from relative-path plans, got: {report}"
    )


def test_triage_health_emits_well_formed_json(tmp_graph):
    """Empty graph produces a valid JSON report with zeroed totals."""
    res = _invoke("backlog", "triage", "health", "--all", "--json")
    assert res.exit_code == 0, res.output
    report = json.loads(res.output)
    assert isinstance(report, dict)
    for key in ("idea_pile_depth", "stale_ready_nodes", "failure_prone_nodes",
                "collisions", "acknowledged_resolved", "totals"):
        assert key in report


# ---------------------------------------------------------------------------
# Unevaluated surface: "nothing to compare" != "compared, clean"
# ---------------------------------------------------------------------------


def _write_ownership_map_plan(path: Path, files: list[str]) -> Path:
    rows = "\n".join(f"| `{f}` | modify | /blueprint |" for f in files)
    path.write_text(
        "# Plan\n\n## File Ownership Map\n\n"
        f"| File | Action | Owner |\n|---|---|---|\n{rows}\n"
    )
    return path


def test_file_ownership_map_is_a_parseable_surface(tmp_path):
    """/blueprint writes File Ownership Map, not Files to Modify; the parser
    must read it or every blueprint-generated plan is invisible to collisions."""
    from fno.graph.collision import parse_files_to_modify

    p = _write_ownership_map_plan(tmp_path / "p.md", ["cli/src/fno/graph/cli.py"])

    assert parse_files_to_modify(p) == {"cli/src/fno/graph/cli.py"}


def test_has_file_surface_distinguishes_empty_from_clean(tmp_path):
    from fno.graph.collision import has_file_surface

    empty = tmp_path / "empty.md"
    empty.write_text("# Plan\n\n## Context\n\nNo file table here.\n")
    populated = _write_ownership_map_plan(tmp_path / "full.md", ["a.py"])

    assert has_file_surface(empty) is False
    assert has_file_surface(populated) is True


def test_collisions_check_json_reports_unevaluated(tmp_graph, tmp_path):
    plan = tmp_path / "surfaceless.md"
    plan.write_text("# Plan\n\n## Context\n\nNothing to compare.\n")

    res = _invoke("backlog", "collisions", "check", str(plan), "--json")

    assert res.exit_code == 0
    payload = json.loads(res.stdout)
    assert payload["status"] == "unevaluated"
    assert payload["collisions"] == []


def test_collisions_check_json_reports_ok_when_clean(tmp_graph, tmp_path):
    plan = _write_ownership_map_plan(tmp_path / "clean.md", ["only/mine.py"])

    res = _invoke("backlog", "collisions", "check", str(plan), "--json")

    assert res.exit_code == 0
    payload = json.loads(res.stdout)
    assert payload["status"] == "ok"
    assert payload["collisions"] == []


def test_collisions_check_human_output_says_unevaluated(tmp_graph, tmp_path):
    """The non-JSON path must not read as 'no collisions found'."""
    plan = tmp_path / "surfaceless.md"
    plan.write_text("# Plan\n\n## Context\n\nNothing to compare.\n")

    res = _invoke("backlog", "collisions", "check", str(plan))

    assert res.exit_code == 0
    assert "UNEVALUATED" in res.output
    assert "No collisions found" not in res.output


def test_parse_folder_plan_scans_index_and_phase_files(tmp_path):
    """Reading a directory raises IsADirectoryError, which degrades to an empty
    set - indistinguishable from a plan stating no surface, so every folder plan
    bypassed the gate."""
    from fno.graph.collision import has_file_surface, parse_files_to_modify

    folder = tmp_path / "plan"
    folder.mkdir()
    _write_quick_plan(folder / "00-INDEX.md", ["src/index.py"])
    _write_quick_plan(folder / "01-phase.md", ["src/phase.py"])

    assert parse_files_to_modify(folder) == {"src/index.py", "src/phase.py"}
    assert has_file_surface(folder) is True
