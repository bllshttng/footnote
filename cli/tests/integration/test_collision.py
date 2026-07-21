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
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


def _invoke(*args, input=None):
    return runner.invoke(app, list(args), input=input, catch_exceptions=False)


def _read_entries(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


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

    res = _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-old", "--reason", "consolidated")
    assert res.exit_code == 0, res.output

    entries = _read_entries(tmp_graph)
    by_id = {e["id"]: e for e in entries}
    assert by_id["ab-old"]["superseded_by"] == "ab-new"
    assert "ab-old" in by_id["ab-new"]["supersedes"]


def test_supersede_defers_old_and_status_becomes_superseded(tmp_graph, tmp_path):
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["x.py", "y.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-old", "--reason", "consolidated")
    assert res.exit_code == 0, res.output

    entries = _read_entries(tmp_graph)
    by_id = {e["id"]: e for e in entries}
    assert by_id["ab-old"]["deferred_at"] is not None
    assert "superseded by ab-new" in by_id["ab-old"]["deferred_reason"]
    # status derivation: superseded_by wins over deferred_at
    assert by_id["ab-old"]["status"] == "superseded"


def test_supersede_done_node_rejected(tmp_graph, tmp_path):
    """Superseding a shipped node would erase its completed_at and destroy
    forensic history. Refuse the mutation; user opens a follow-up instead."""
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-shipped", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["y.py"])))
    entries[0]["completed_at"] = "2026-04-30T12:00:00+00:00"
    entries[0]["status"] = "done"
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-shipped", "--reason", "test")
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

    res = _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-old", "--reason", "test")
    assert res.exit_code != 0
    assert "already superseded" in res.output.lower()


def test_supersede_self_rejected(tmp_graph, tmp_path):
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-x", plan_path=str(_write_quick_plan(tmp_path / "x.md", ["x.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "supersede", "ab-x", "--replaces", "ab-x", "--reason", "test")
    assert res.exit_code != 0
    assert "supersede self" in res.output


def test_supersede_blank_reason_rejected(tmp_graph, tmp_path):
    entries = _read_entries(tmp_graph)
    _seed_node(entries, id_="ab-old", plan_path=str(_write_quick_plan(tmp_path / "old.md", ["x.py"])))
    _seed_node(entries, id_="ab-new", plan_path=str(_write_quick_plan(tmp_path / "new.md", ["y.py"])))
    tmp_graph.write_text(json.dumps({"entries": entries}, indent=2))

    res = _invoke("backlog", "supersede", "ab-new", "--replaces", "ab-old", "--reason", "   ")
    assert res.exit_code != 0
    assert "blank" in res.output.lower()


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
