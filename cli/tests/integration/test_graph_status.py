"""Tests for the `idea` derived status and related backlog UX.

Covers:
- The idea status derivation cascade in ``recompute_statuses``
- ``--include-ideas`` filter on ``backlog ready`` / ``backlog next``
- ``backlog status`` idea count line
- ``backlog add`` ``--description`` alias and auto-detect for ``--project``/``--cwd``
- ``backlog idea`` sugar verb for capturing plan-less nodes
- ``backlog triage context`` surfaces ideas in a separate array
- Legacy graph.json rows migrate to idea status on next recompute
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    """Fresh empty graph.json routed to tmp_path."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    return g


def _invoke(*args, input=None):
    return runner.invoke(app, list(args), input=input, catch_exceptions=False)


def _read_entries(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


# ---------------------------------------------------------------------------
# Status derivation cascade
# ---------------------------------------------------------------------------


def test_idea_status_derived_from_no_plan_path(tmp_graph):
    """A node added without a plan_path derives to status: idea."""
    r = _invoke("--json", "backlog", "add", "Just an idea")
    assert r.exit_code == 0, r.output
    node_id = json.loads(r.stdout)["id"]

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("plan_path") is None
    assert node.get("status") == "idea", (
        f"plan-less node should derive to 'idea', got {node.get('status')!r}"
    )


def test_idea_status_overridden_by_in_progress(tmp_graph):
    """An idea-shaped node that gets claimed (session_id set) derives to in_progress."""
    add = _invoke("--json", "backlog", "add", "Claimed idea")
    node_id = json.loads(add.stdout)["id"]

    r = _invoke("backlog", "update", node_id, "--locked-by", "session-X")
    assert r.exit_code == 0, r.output

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("session_id") == "session-X"
    assert node.get("status") == "in_progress", (
        f"in_progress beats idea; got {node.get('status')!r}"
    )


def test_idea_status_overridden_by_blocked(tmp_graph):
    """A plan-less node with an unresolved blocker derives to blocked, not idea."""
    a = _invoke("--json", "backlog", "add", "Blocker A")
    blocker_id = json.loads(a.stdout)["id"]
    b = _invoke("--json", "backlog", "add", "Idea blocked by A", "--blocked-by", blocker_id)
    node_id = json.loads(b.stdout)["id"]

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("plan_path") is None
    assert node.get("status") == "blocked", (
        f"blocked beats idea; got {node.get('status')!r}"
    )


def test_node_with_plan_path_derives_to_ready(tmp_graph, tmp_path):
    """A node with a plan_path (via intake) derives to ready, not idea."""
    plan = tmp_path / "fake-plan.md"
    plan.write_text("---\ntitle: Real Plan\n---\n# Body\n")
    r = _invoke("backlog", "intake", str(plan))
    assert r.exit_code == 0, r.output

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e.get("plan_path"))
    assert node.get("status") == "ready", (
        f"plan-having node should derive to 'ready', got {node.get('status')!r}"
    )


# ---------------------------------------------------------------------------
# --include-ideas filter on ready / next
# ---------------------------------------------------------------------------


def _seed_one_idea_one_ready(tmp_path) -> tuple[str, str]:
    """Helper: add one idea (no plan) and one ready (with plan). Returns (idea_id, ready_id)."""
    idea = _invoke("--json", "backlog", "add", "Pure idea")
    idea_id = json.loads(idea.stdout)["id"]
    plan = tmp_path / "ready-plan.md"
    plan.write_text("---\ntitle: Ready Plan\n---\n# Body\n")
    ready = _invoke("backlog", "intake", str(plan))
    assert ready.exit_code == 0
    return idea_id, ready.stdout


def test_idea_excluded_from_next_by_default(tmp_graph, tmp_path):
    """`backlog next` returns only ready rows, ideas are filtered out."""
    idea_id, _ = _seed_one_idea_one_ready(tmp_path)

    r = _invoke("backlog", "next", "--all")
    assert r.exit_code == 0, r.output
    if r.stdout.strip() == "null":
        pytest.fail("expected the ready node, got null")
    payload = json.loads(r.stdout)
    assert payload is not None
    assert payload.get("id") != idea_id, (
        "idea should be excluded from `next` by default"
    )


def test_idea_included_with_flag(tmp_graph, tmp_path):
    """`backlog next --include-ideas` considers idea rows alongside ready."""
    plan = tmp_path / "low-prio.md"
    plan.write_text("---\ntitle: Low Prio Plan\n---\n# Body\n")
    _invoke("backlog", "intake", str(plan), "--priority", "p3")

    high = _invoke(
        "--json", "backlog", "add", "High-prio idea",
        "--priority", "p1",
    )
    assert high.exit_code == 0
    idea_id = json.loads(high.stdout)["id"]

    r = _invoke("backlog", "next", "--all", "--include-ideas")
    assert r.exit_code == 0, r.output
    payload = json.loads(r.stdout)
    assert payload is not None
    assert payload.get("id") == idea_id, (
        f"high-prio idea should win when --include-ideas is set, got {payload}"
    )


def test_idea_excluded_from_ready_listing_by_default(tmp_graph, tmp_path):
    """`backlog ready` listing omits ideas by default."""
    idea_id, _ = _seed_one_idea_one_ready(tmp_path)

    r = _invoke("backlog", "ready", "--all")
    assert r.exit_code == 0, r.output
    listing = json.loads(r.stdout)
    ids = [e["id"] for e in listing]
    assert idea_id not in ids, "ideas should not appear in default `ready` listing"


def test_idea_included_in_ready_with_flag(tmp_graph, tmp_path):
    """`backlog ready --include-ideas` surfaces both ready and idea rows."""
    idea_id, _ = _seed_one_idea_one_ready(tmp_path)

    r = _invoke("backlog", "ready", "--all", "--include-ideas")
    assert r.exit_code == 0, r.output
    listing = json.loads(r.stdout)
    ids = [e["id"] for e in listing]
    assert idea_id in ids, "ideas should appear when --include-ideas is set"


# ---------------------------------------------------------------------------
# status summary idea count
# ---------------------------------------------------------------------------


def test_status_summary_shows_idea_count(tmp_graph, tmp_path):
    """`backlog status` prints `ideas: N` after the ready count."""
    _invoke("backlog", "add", "Idea one")
    plan = tmp_path / "plan.md"
    plan.write_text("---\ntitle: A Plan\n---\n# Body\n")
    _invoke("backlog", "intake", str(plan))

    r = _invoke("backlog", "status", "--all")
    assert r.exit_code == 0, r.output
    assert "ideas:" in r.output.lower() or "ideas " in r.output.lower(), (
        f"status output should mention ideas; got:\n{r.output}"
    )
    assert "1" in r.output, "should show count of 1 idea"


# ---------------------------------------------------------------------------
# --description alias and project/cwd auto-detect
# ---------------------------------------------------------------------------


def test_description_flag_aliases_details(tmp_graph):
    """`--description X` and `--details X` produce the same node."""
    r1 = _invoke("--json", "backlog", "add", "Has description", "--description", "a description")
    r2 = _invoke("--json", "backlog", "add", "Has details", "--details", "a description")
    assert r1.exit_code == 0, r1.output
    assert r2.exit_code == 0, r2.output

    entries = _read_entries(tmp_graph)
    by_title = {e["title"]: e for e in entries}
    assert by_title["Has description"]["details"] == "a description"
    assert by_title["Has details"]["details"] == "a description"


def test_description_and_details_mutually_exclusive(tmp_graph):
    """Passing both --description and --details errors out."""
    r = _invoke(
        "backlog", "add", "Both flags",
        "--description", "A",
        "--details", "B",
    )
    assert r.exit_code != 0, "should reject when both flags are passed"
    combined = (r.stdout + (r.stderr or "")).lower()
    assert "description" in combined and "details" in combined


def test_auto_detect_project_from_settings(tmp_graph, tmp_path, monkeypatch):
    """When `pwd` matches a path in settings.yaml work config, project is auto-filled."""
    cwd = tmp_path / "myrepo"
    cwd.mkdir()
    settings = tmp_path / "settings.yaml"
    settings.write_text(
        "work:\n"
        "  workspaces:\n"
        "    home:\n"
        "      projects:\n"
        f"        - name: detected-proj\n"
        f"          path: {cwd}\n"
    )

    monkeypatch.chdir(cwd)
    monkeypatch.setenv("HOME", str(tmp_path))
    # Make .fno/settings.yaml resolve to our settings file at the project root.
    abilities_dir = cwd / ".fno"
    abilities_dir.mkdir()
    (abilities_dir / "settings.yaml").write_text(settings.read_text())

    r = _invoke("--json", "backlog", "add", "Auto-detect test")
    assert r.exit_code == 0, r.output
    node_id = json.loads(r.stdout)["id"]

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("project") == "detected-proj", (
        f"expected auto-detected project 'detected-proj', got {node.get('project')!r}"
    )
    assert node.get("cwd") == str(cwd), (
        f"expected auto-detected cwd {str(cwd)!r}, got {node.get('cwd')!r}"
    )


def test_explicit_project_overrides_auto_detect(tmp_graph, tmp_path, monkeypatch):
    """`--project foo` always wins, even when settings.yaml would auto-fill differently."""
    cwd = tmp_path / "myrepo"
    cwd.mkdir()
    settings_dir = cwd / ".fno"
    settings_dir.mkdir()
    (settings_dir / "settings.yaml").write_text(
        "work:\n"
        "  workspaces:\n"
        "    home:\n"
        "      projects:\n"
        "        - name: detected-proj\n"
        f"          path: {cwd}\n"
    )

    monkeypatch.chdir(cwd)
    monkeypatch.setenv("HOME", str(tmp_path))

    r = _invoke("--json", "backlog", "add", "Explicit override", "--project", "explicit-proj")
    assert r.exit_code == 0, r.output
    node_id = json.loads(r.stdout)["id"]

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("project") == "explicit-proj"


def test_relative_cwd_resolves_to_absolute(tmp_graph, tmp_path, monkeypatch):
    """A relative `--cwd .` is resolved to an absolute path before settings
    lookup, so it matches absolute paths declared in settings.yaml.
    """
    cwd = tmp_path / "relrepo"
    cwd.mkdir()
    abilities_dir = cwd / ".fno"
    abilities_dir.mkdir()
    (abilities_dir / "settings.yaml").write_text(
        "work:\n"
        "  workspaces:\n"
        "    home:\n"
        "      projects:\n"
        "        - name: rel-detected\n"
        f"          path: {cwd}\n"
    )

    monkeypatch.chdir(cwd)
    monkeypatch.setenv("HOME", str(tmp_path))

    # Pass relative `--cwd .` and verify the stored cwd is absolute and
    # the project auto-detected via the absolute target.
    r = _invoke("--json", "backlog", "add", "Relative cwd test", "--cwd", ".")
    assert r.exit_code == 0, r.output
    node_id = json.loads(r.stdout)["id"]

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert os.path.isabs(node.get("cwd") or ""), (
        f"stored cwd should be absolute; got {node.get('cwd')!r}"
    )
    assert node.get("project") == "rel-detected", (
        f"relative `.` should resolve to the registered project; got {node.get('project')!r}"
    )


def test_no_match_in_settings_leaves_project_none(tmp_graph, tmp_path, monkeypatch):
    """Auto-detect is silent when no settings.yaml entry matches the cwd."""
    cwd = tmp_path / "unknown-repo"
    cwd.mkdir()
    monkeypatch.chdir(cwd)
    monkeypatch.setenv("HOME", str(tmp_path))

    r = _invoke("--json", "backlog", "add", "No-match test")
    assert r.exit_code == 0, r.output
    node_id = json.loads(r.stdout)["id"]

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("project") is None, (
        f"unmatched cwd should leave project None, got {node.get('project')!r}"
    )


def test_global_settings_consulted_when_inside_project(tmp_path, monkeypatch):
    """The work.workspaces map lives only in global ~/.fno/settings.yaml.

    Inside a repo, config_file() resolves to the project-local settings, so
    without the global file in the candidate list detect returns None for every
    node filed from inside a project (ab-95e8efec). The sibling reader
    _list_known_projects() must agree, or a correctly-attributed node would
    trip a spurious "unknown project" warning.
    """
    import fno.graph._intake as intake

    # The repo we are filing from: NO project-local .fno/settings.yaml.
    repo = tmp_path / "code" / "myrepo"
    repo.mkdir(parents=True)

    # The work-map lives ONLY in global ~/.fno/settings.yaml.
    home = tmp_path / "home"
    (home / ".fno").mkdir(parents=True)
    (home / ".fno" / "settings.yaml").write_text(
        "work:\n"
        "  workspaces:\n"
        "    home:\n"
        "      projects:\n"
        "        - name: myrepo\n"
        f"          path: {repo}\n"
    )
    # Point the global settings resolver at the work-map, overriding conftest's
    # FNO_GLOBAL_SETTINGS_PATH=/dev/null pin. config_file() is pinned to a
    # project-local file that lacks the work-map - the live "inside a repo"
    # condition - and we run from inside the repo so the cwd-relative candidate
    # also misses. The global fallback is then the only candidate that matches.
    monkeypatch.setenv(
        "FNO_GLOBAL_SETTINGS_PATH", str(home / ".fno" / "settings.yaml")
    )
    monkeypatch.setattr(
        intake._paths, "config_file", lambda: repo / ".fno" / "settings.yaml"
    )
    monkeypatch.chdir(repo)

    assert intake.detect_project_from_settings(str(repo)) == "myrepo"
    assert "myrepo" in intake._list_known_projects()


# ---------------------------------------------------------------------------
# Migration: legacy ready rows flip to idea on next recompute
# ---------------------------------------------------------------------------


def test_legacy_ready_row_migrates_to_idea(tmp_graph):
    """Pre-existing graph.json rows with `plan_path: None, status: "ready"`
    flip to `status: "idea"` after the next mutation triggers
    `recompute_statuses()`.

    This locks in plan verification step 8: existing rows with no
    plan_path and otherwise-ready state should automatically migrate to
    the new idea bucket without a schema change.
    """
    # Seed a graph that pretends to predate this feature: a "ready" row
    # with no plan_path. Real legacy graph.json files have exactly this
    # shape because pre-feature `add` set plan_path=None and the old
    # cascade derived status="ready".
    tmp_graph.write_text(json.dumps({
        "entries": [
            {
                "id": "ab-legacy01",
                "parent": None,
                "title": "Legacy ready row",
                "type": "feature",
                "project": None,
                "cwd": None,
                "priority": "medium",
                "domain": "code",
                "blocked_by": [],
                "session_id": None,
                "claimed_at": None,
                "completed_at": None,
                "has_brief": False,
                "compacted": False,
                "roadmap_id": None,
                "vision_path": None,
                "details": None,
                "size": None,
                "batch": None,
                "cost_usd": None,
                "cost_sessions": [],
                "plan_path": None,
                "pr_number": None,
                "pr_url": None,
                "merge_status": None,
                "status": "ready",  # the pre-feature derivation
                "created_at": "2026-04-01T00:00:00+00:00",
            }
        ]
    }))

    # Trigger any mutation - locked_mutate_graph runs recompute_statuses
    # on every successful mutation, which is what the plan promises.
    r = _invoke("backlog", "add", "Trigger mutation")
    assert r.exit_code == 0, r.output

    entries = _read_entries(tmp_graph)
    legacy = next(e for e in entries if e["id"] == "ab-legacy01")
    assert legacy.get("status") == "idea", (
        f"legacy ready-with-no-plan row should migrate to idea on next "
        f"recompute; got {legacy.get('status')!r}"
    )


# ---------------------------------------------------------------------------
# triage context separates ideas
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# Shorthands: -A for --all, -I / --ideas for --include-ideas
# ---------------------------------------------------------------------------


def test_dash_a_is_shorthand_for_all_in_ready(tmp_graph, tmp_path):
    """`backlog ready -A` is equivalent to `--all`."""
    plan = tmp_path / "p.md"
    plan.write_text("---\ntitle: Plan\n---\n# Body\n")
    _invoke("backlog", "intake", str(plan))
    r = _invoke("backlog", "ready", "-A")
    assert r.exit_code == 0, r.output
    listing = json.loads(r.stdout)
    assert isinstance(listing, list) and len(listing) == 1


def test_dash_a_is_shorthand_for_all_in_next(tmp_graph, tmp_path):
    """`backlog next -A` is equivalent to `--all`."""
    plan = tmp_path / "p.md"
    plan.write_text("---\ntitle: Plan\n---\n# Body\n")
    _invoke("backlog", "intake", str(plan))
    r = _invoke("backlog", "next", "-A")
    assert r.exit_code == 0, r.output
    payload = json.loads(r.stdout)
    assert payload is not None


def test_dash_a_is_shorthand_for_all_in_status(tmp_graph):
    """`backlog status -A` is equivalent to `--all`."""
    _invoke("backlog", "add", "Idea")
    r = _invoke("backlog", "status", "-A")
    assert r.exit_code == 0, r.output


def test_ideas_long_flag_accepted_for_ready(tmp_graph, tmp_path):
    """`--ideas` is accepted as the user-facing long form for include-ideas."""
    idea_id, _ = _seed_one_idea_one_ready(tmp_path)
    r = _invoke("backlog", "ready", "--all", "--ideas")
    assert r.exit_code == 0, r.output
    ids = [e["id"] for e in json.loads(r.stdout)]
    assert idea_id in ids


def test_dash_i_is_shorthand_for_ideas_in_next(tmp_graph, tmp_path):
    """`-I` is the short flag for --ideas (alias of --include-ideas)."""
    high = _invoke(
        "--json", "backlog", "add", "High idea",
        "--priority", "p1",
    )
    high_id = json.loads(high.stdout)["id"]
    r = _invoke("backlog", "next", "--all", "-I")
    assert r.exit_code == 0, r.output
    payload = json.loads(r.stdout)
    assert payload is not None
    assert payload["id"] == high_id


def test_include_ideas_long_flag_still_works(tmp_graph, tmp_path):
    """`--include-ideas` remains a working alias (back-compat with the spec doc)."""
    idea_id, _ = _seed_one_idea_one_ready(tmp_path)
    r = _invoke("backlog", "ready", "--all", "--include-ideas")
    assert r.exit_code == 0, r.output
    ids = [e["id"] for e in json.loads(r.stdout)]
    assert idea_id in ids


# ---------------------------------------------------------------------------
# `backlog idea` shorthand verb
# ---------------------------------------------------------------------------


def test_backlog_idea_creates_plan_less_node(tmp_graph):
    """`backlog idea "X"` creates an idea-stage node (no plan_path)."""
    r = _invoke("--json", "backlog", "idea", "Capture this thought")
    assert r.exit_code == 0, r.output
    payload = json.loads(r.stdout)
    node_id = payload["id"]

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node["title"] == "Capture this thought"
    assert node.get("plan_path") is None
    assert node.get("status") == "idea"


def test_backlog_idea_accepts_description(tmp_graph):
    """`backlog idea "X" --description "Y"` stores Y in details."""
    r = _invoke(
        "--json", "backlog", "idea", "Idea with body",
        "--description", "explain the idea here",
    )
    assert r.exit_code == 0, r.output
    node_id = json.loads(r.stdout)["id"]
    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node["details"] == "explain the idea here"


def test_backlog_idea_accepts_priority(tmp_graph):
    """`backlog idea "X" --priority p1` is honored."""
    r = _invoke(
        "--json", "backlog", "idea", "Urgent idea",
        "--priority", "p1",
    )
    assert r.exit_code == 0, r.output
    node_id = json.loads(r.stdout)["id"]
    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node["priority"] == "p1"


def test_backlog_idea_listed_in_help():
    """`backlog --help` advertises the new `idea` verb."""
    r = _invoke("backlog", "--help")
    assert r.exit_code == 0
    assert "idea" in r.output


def test_triage_context_separates_ideas_from_candidates(tmp_graph, tmp_path):
    """`backlog triage context` surfaces ideas in their own array, not in candidates."""
    idea_id, _ = _seed_one_idea_one_ready(tmp_path)

    r = _invoke("backlog", "triage", "context", "--all")
    assert r.exit_code == 0, r.output
    ctx = json.loads(r.stdout)

    candidate_ids = [c["id"] for c in ctx.get("candidates", [])]
    assert idea_id not in candidate_ids, (
        "ideas must not appear in `candidates` (those are claim-ready rows)"
    )

    assert "ideas" in ctx, f"context should expose `ideas` key; got keys {list(ctx.keys())}"
    idea_ids = [i["id"] for i in ctx["ideas"]]
    assert idea_id in idea_ids, (
        "the unspec'd node should appear in the `ideas` array so the LLM can recommend specing it"
    )
