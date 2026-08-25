"""Tests for `fno backlog decompose` - bounded epic decomposition (ab-e9c81ed3, C1).

The verb upserts group child nodes under an epic in a single locked graph
mutation: atomic (all-or-nothing) and idempotent (keyed on parent + the group
slug). Each child gets its own self-contained <stem>.group-<slug>.md quick-plan
(separate packaging is the only packaging; the legacy `#group-<slug>` fragment
is still recognized on existing children but never authored). Covers AC1-HP,
AC1-ERR, AC1-UI, AC1-EDGE, AC1-FR from
internal/fno/plans/2026-05-24-epic-scoped-execution.md.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner


# -- fixtures --


def _node(node_id: str, **overrides) -> dict:
    base = {
        "id": node_id,
        "parent": None,
        "title": "default-title",
        "type": "feature",
        "project": "fno",
        "cwd": "/tmp/fno",
        "priority": "p2",
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
        "artifact_url": None,
        "completion_note": None,
        "status": "idea",
        "created_at": "2026-01-01T00:00:00+00:00",
    }
    base.update(overrides)
    return base


@pytest.fixture
def graph_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Temp graph.json wired into the CLI; returns (path, read_entries).

    The epic's plan_path points at a real doc under tmp_path so separate-mode
    (the only packaging) scaffolds each child's <stem>.group-<slug>.md beside it,
    inside the test's tmp dir - never polluting the repo tree.
    """
    import fno.graph._constants as gc
    import fno.graph.store as gs

    doc = tmp_path / "big.md"
    doc.write_text("---\ntitle: Big epic\nstatus: draft\n---\n# body\n")

    g = tmp_path / "graph.json"
    epic = _node(
        "ab-epic0001",
        title="Epic: big thing",
        plan_path=f"{doc}#c1-anchor",
        priority="p1",
        project="fno",
        cwd=str(tmp_path),
        status="ready",
    )
    g.write_text(json.dumps({"entries": [epic]}) + "\n")

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)

    def read_entries():
        return json.loads(g.read_text())["entries"]

    return g, read_entries


def _groups_json(groups) -> str:
    return json.dumps(groups)


def _child(entries, slug):
    """The group child with the given slug (x-edf7: identity is group_slug, not
    plan_path - children are born unlinked until inline-fill links a real plan)."""
    return next(e for e in entries if e.get("group_slug") == slug)


def _invoke(args, input_text=None):
    from fno.cli import app

    return CliRunner().invoke(app, args, input=input_text)


THREE_GROUPS = [
    {"slug": "1", "title": "Group 1: foundation", "waves": "1-3", "blocked_by_groups": []},
    {"slug": "2", "title": "Group 2: api", "waves": "4-5", "blocked_by_groups": ["1"]},
    {"slug": "3", "title": "Group 3: ui", "waves": "6", "blocked_by_groups": ["2"]},
]


# -- AC1-HP: bounded decomposition --


def test_ac1_hp_creates_group_children(graph_env):
    g, read_entries = graph_env
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)]
    )
    assert result.exit_code == 0, result.output

    entries = read_entries()
    children = [e for e in entries if e.get("parent") == "ab-epic0001"]
    assert len(children) == 3

    # x-edf7 US2: children are born UNLINKED (no plan_path), identified by
    # group_slug. Linking a filled plan is the later fill step, so NO child is
    # `ready` yet. The unblocked group derives `idea`; a group with an open
    # inter-group blocker derives `blocked` - never `ready`.
    assert {c["group_slug"] for c in children} == {"1", "2", "3"}
    for c in children:
        assert c["parent"] == "ab-epic0001"
        assert c["plan_path"] is None
        assert c["status"] != "ready"
    assert _child(children, "1")["status"] == "idea"  # no blockers -> idea


def test_ac1_hp_inter_group_blocked_by_resolves_to_ids(graph_env):
    g, read_entries = graph_env
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])

    entries = read_entries()
    g1 = _child(entries, "1")
    g2 = _child(entries, "2")
    g3 = _child(entries, "3")

    assert g1["blocked_by"] == []
    assert g2["blocked_by"] == [g1["id"]]
    assert g3["blocked_by"] == [g2["id"]]


def test_wave_range_persisted_to_details(graph_env):
    g, read_entries = graph_env
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])
    g1 = _child(read_entries(), "1")
    # AC1-UI wave range is not just echoed - it persists on the child node.
    assert "1-3" in (g1.get("details") or "")


def test_inherits_epic_project_cwd(graph_env, tmp_path):
    g, read_entries = graph_env
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])
    children = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    for c in children:
        assert c["project"] == "fno"
        assert c["cwd"] == str(tmp_path)


# -- per-group repo routing (multi-repo decomposition) --


def _patch_workmap(monkeypatch, mapping):
    """Stub project_root_from_settings so a known project resolves to a root."""
    import fno.graph._intake as intake

    monkeypatch.setattr(
        intake, "project_root_from_settings", lambda p: mapping.get(p)
    )


def test_per_group_project_derives_cwd_from_workmap(graph_env, tmp_path, monkeypatch):
    """A group with `project` routes the child into that repo, cwd from work-map."""
    g, read_entries = graph_env
    _patch_workmap(monkeypatch, {"web": "/repos/web"})
    groups = [
        {"slug": "1", "title": "G1 backend", "waves": "1", "blocked_by_groups": []},
        {"slug": "2", "title": "G2 web", "waves": "2", "blocked_by_groups": ["1"],
         "project": "web"},
    ]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output
    children = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    g1 = _child(children, "1")
    g2 = _child(children, "2")
    # G1 inherits the epic's repo; G2 routed into web.
    assert (g1["project"], g1["cwd"]) == ("fno", str(tmp_path))
    assert (g2["project"], g2["cwd"]) == ("web", "/repos/web")


def test_per_group_explicit_cwd_used(graph_env, monkeypatch):
    """An explicit cwd is used verbatim (abspath); project still inherits epic."""
    g, read_entries = graph_env
    _patch_workmap(monkeypatch, {})  # work-map not consulted when cwd is explicit
    groups = [
        {"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": [],
         "cwd": "/custom/root"},
    ]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output
    child = next(e for e in read_entries() if e.get("parent") == "ab-epic0001")
    assert child["cwd"] == "/custom/root"
    assert child["project"] == "fno"  # inherited (no explicit project)


def test_per_group_unmapped_project_refused_atomically(graph_env, monkeypatch):
    """An unmapped project is refused before any write (atomic), not guessed."""
    g, read_entries = graph_env
    _patch_workmap(monkeypatch, {})  # nothing resolves
    groups = [
        {"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []},
        {"slug": "2", "title": "G2", "waves": "2", "blocked_by_groups": ["1"],
         "project": "ghost"},
    ]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(groups)])
    assert result.exit_code == 1
    assert "work-map" in result.output
    # Atomic: no children created despite the first group being valid.
    assert [e for e in read_entries() if e.get("parent") == "ab-epic0001"] == []


def test_redecompose_adds_route_reprojects_existing(graph_env, monkeypatch):
    """A second pass that adds a route reprojects the already-created child."""
    g, read_entries = graph_env
    _patch_workmap(monkeypatch, {"web": "/repos/web"})
    base = [{"slug": "2", "title": "G2", "waves": "2", "blocked_by_groups": []}]
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(base)])
    child = next(e for e in read_entries() if e.get("parent") == "ab-epic0001")
    assert child["project"] == "fno"  # inherited on first pass

    routed = [{"slug": "2", "title": "G2", "waves": "2", "blocked_by_groups": [],
               "project": "web"}]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(routed)])
    assert result.exit_code == 0, result.output
    children = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    assert len(children) == 1  # upsert, not duplicate
    assert (children[0]["project"], children[0]["cwd"]) == ("web", "/repos/web")


def test_invalid_project_type_rejected(graph_env):
    """A non-string project is a spec error (exit 1), nothing written."""
    g, read_entries = graph_env
    groups = [{"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": [],
               "project": 123}]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(groups)])
    assert result.exit_code == 1
    assert [e for e in read_entries() if e.get("parent") == "ab-epic0001"] == []


# -- AC1-UI: command feedback --


def test_ac1_ui_lists_epic_and_children(graph_env):
    g, read_entries = graph_env
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)]
    )
    assert result.exit_code == 0
    out = result.output
    assert "ab-epic0001" in out
    # each created child id appears with its wave range
    children = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    for c in children:
        assert c["id"] in out
    assert "1-3" in out and "4-5" in out and "6" in out


def test_json_output_shape(graph_env):
    g, read_entries = graph_env
    result = _invoke(
        ["backlog", "--json", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)]
    )
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["epic"] == "ab-epic0001"
    assert len(payload["groups"]) == 3
    assert {g["slug"] for g in payload["groups"]} == {"1", "2", "3"}
    for grp in payload["groups"]:
        assert grp["id"].startswith("ab-")
        assert grp["action"] in ("created", "updated")


# -- AC1-ERR: invalid budget --


def test_ac1_err_max_prs_zero_creates_nothing(graph_env):
    g, read_entries = graph_env
    before = read_entries()
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--max-prs", "0",
         "--groups", _groups_json(THREE_GROUPS)]
    )
    assert result.exit_code != 0
    assert read_entries() == before  # nothing created


def test_ac1_err_groups_exceed_ceiling(graph_env):
    g, read_entries = graph_env
    before = read_entries()
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--max-prs", "2",
         "--groups", _groups_json(THREE_GROUPS)]
    )
    assert result.exit_code != 0
    assert "ceiling" in result.output.lower() or "max" in result.output.lower()
    assert read_entries() == before


def test_empty_groups_rejected(graph_env):
    g, read_entries = graph_env
    before = read_entries()
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", "[]"])
    assert result.exit_code != 0
    assert read_entries() == before


def test_bad_epic_id_exits_not_found(graph_env):
    g, read_entries = graph_env
    before = read_entries()
    result = _invoke(
        ["backlog", "decompose", "ab-nosuch99", "--groups", _groups_json(THREE_GROUPS)]
    )
    assert result.exit_code != 0
    assert read_entries() == before


# -- AC1-EDGE: no forced splitting (ceiling, not quota) --


def test_ac1_edge_fewer_groups_than_ceiling(graph_env):
    g, read_entries = graph_env
    two = [
        {"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []},
        {"slug": "2", "title": "G2", "waves": "2", "blocked_by_groups": ["1"]},
    ]
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--max-prs", "5", "--groups", _groups_json(two)]
    )
    assert result.exit_code == 0, result.output
    children = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    assert len(children) == 2  # never padded to 5


# -- AC1-FR / US4: atomic, idempotent re-decompose --


def test_ac2_edge_redecompose_preserves_filled_child_plan_path(graph_env):
    """x-edf7 AC2-EDGE: a child that was inline-filled + linked keeps its
    plan_path across re-decompose; a designed plan is never unset or clobbered."""
    g, read_entries = graph_env
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])

    # Simulate group 1 being inline-filled + linked (the design-completion event).
    entries = read_entries()
    child1 = _child(entries, "1")
    filled_path = "/plans/big.group-1.md"
    child1["plan_path"] = filled_path
    Path(g).write_text(json.dumps({"entries": entries}) + "\n")

    # Re-decompose with an edited group set (titles bumped).
    changed = [dict(grp, title=grp["title"] + " v2") for grp in THREE_GROUPS]
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(changed)])

    after = read_entries()
    assert _child(after, "1")["plan_path"] == filled_path   # designed plan untouched
    assert _child(after, "1")["status"] == "ready"          # stays ready
    # An unfilled sibling stays unlinked - re-decompose never spuriously links it.
    assert _child(after, "2")["plan_path"] is None


def test_us4_rerun_updates_in_place_no_duplicates(graph_env):
    g, read_entries = graph_env
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])
    first = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    first_ids = sorted(e["id"] for e in first)

    # Re-run with same slugs but changed titles
    changed = [dict(grp, title=grp["title"] + " (v2)") for grp in THREE_GROUPS]
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(changed)])

    second = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    assert sorted(e["id"] for e in second) == first_ids  # same nodes, no dupes
    titles = {e["title"] for e in second}
    assert all("(v2)" in t for t in titles)


def test_ac1_fr_atomic_on_bad_reference(graph_env):
    g, read_entries = graph_env
    before = read_entries()
    bad = [
        {"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []},
        {"slug": "2", "title": "G2", "waves": "2", "blocked_by_groups": ["nonexistent"]},
    ]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(bad)])
    assert result.exit_code != 0
    # graph unchanged: no partial children left behind
    assert read_entries() == before


def test_inter_group_cycle_rejected(graph_env):
    g, read_entries = graph_env
    before = read_entries()
    cyclic = [
        {"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": ["2"]},
        {"slug": "2", "title": "G2", "waves": "2", "blocked_by_groups": ["1"]},
    ]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(cyclic)])
    assert result.exit_code == 2  # documented bad-state/cycle exit code
    assert "cycle" in result.output.lower()
    assert read_entries() == before


def test_duplicate_slug_rejected(graph_env):
    g, read_entries = graph_env
    before = read_entries()
    dupe = [
        {"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []},
        {"slug": "1", "title": "G1 again", "waves": "2", "blocked_by_groups": []},
    ]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(dupe)])
    assert result.exit_code != 0
    assert read_entries() == before


def test_bad_slug_chars_rejected(graph_env):
    g, read_entries = graph_env
    before = read_entries()
    bad = [{"slug": "has space", "title": "G", "waves": "1", "blocked_by_groups": []}]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(bad)])
    assert result.exit_code != 0
    assert read_entries() == before


# -- groups arg sources: stdin and @file --


def test_groups_from_stdin(graph_env):
    g, read_entries = graph_env
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", "-"],
        input_text=_groups_json(THREE_GROUPS),
    )
    assert result.exit_code == 0, result.output
    assert len([e for e in read_entries() if e.get("parent") == "ab-epic0001"]) == 3


def test_groups_from_file(graph_env, tmp_path):
    g, read_entries = graph_env
    spec = tmp_path / "groups.json"
    spec.write_text(_groups_json(THREE_GROUPS))
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", f"@{spec}"]
    )
    assert result.exit_code == 0, result.output
    assert len([e for e in read_entries() if e.get("parent") == "ab-epic0001"]) == 3


def test_invalid_json_literal_reports_parse_error(graph_env):
    g, read_entries = graph_env
    before = read_entries()
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", "{not json"])
    assert result.exit_code != 0
    assert "json" in result.output.lower()
    assert read_entries() == before


# -- re-decompose orphan handling (plan Errors invariant, line 84) --


def test_redecompose_dropping_unshipped_group_warns_and_keeps(graph_env):
    g, read_entries = graph_env
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])
    # Re-run with only group 1; groups 2 and 3 become orphans (unshipped).
    one = [{"slug": "1", "title": "G1", "waves": "1-3", "blocked_by_groups": []}]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(one)])
    assert result.exit_code == 0, result.output
    # Orphans are left in place (not deleted), and reported.
    children = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    assert len(children) == 3
    assert "orphan" in result.output.lower() or "left in place" in result.output.lower()


def test_redecompose_orphaning_shipped_group_rejected(graph_env):
    g, read_entries = graph_env
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])
    # Mark group 3 as shipped, then try to drop it.
    entries = read_entries()
    g3 = _child(entries, "3")
    g3["pr_number"] = 999
    g.write_text(json.dumps({"entries": entries}) + "\n")
    before = read_entries()

    two = [
        {"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []},
        {"slug": "2", "title": "G2", "waves": "2", "blocked_by_groups": ["1"]},
    ]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(two)])
    assert result.exit_code == 2
    assert "shipped" in result.output.lower()
    assert read_entries() == before  # nothing changed


def test_redecompose_orphaning_shipped_group_allowed_with_force(graph_env):
    g, read_entries = graph_env
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])
    entries = read_entries()
    g3 = _child(entries, "3")
    g3["pr_number"] = 999
    g.write_text(json.dumps({"entries": entries}) + "\n")

    two = [
        {"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []},
        {"slug": "2", "title": "G2", "waves": "2", "blocked_by_groups": ["1"]},
    ]
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--force", "--groups", _groups_json(two)]
    )
    assert result.exit_code == 0, result.output


# -- config.blueprint.max_prs_per_epic fallback when --max-prs omitted --


def test_config_fallback_ceiling_applied(graph_env, tmp_path, monkeypatch):
    """With no --max-prs, the config default (2) is enforced as the ceiling."""
    g, read_entries = graph_env
    settings_dir = tmp_path / ".fno"
    settings_dir.mkdir(parents=True, exist_ok=True)
    settings_file = settings_dir / "settings.yaml"
    settings_file.write_text(
        "schema_version: 1\nconfig:\n  blueprint:\n    max_prs_per_epic: 2\n"
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    before = read_entries()
    # 3 groups exceed the config ceiling of 2 -> rejected, nothing created.
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])
    assert result.exit_code != 0
    assert read_entries() == before
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]


def test_invalid_config_ceiling_surfaced_not_swallowed(graph_env, tmp_path, monkeypatch):
    """An invalid config.blueprint.max_prs_per_epic surfaces, not silently -> 4."""
    g, read_entries = graph_env
    settings_dir = tmp_path / ".fno"
    settings_dir.mkdir(parents=True, exist_ok=True)
    settings_file = settings_dir / "settings.yaml"
    settings_file.write_text(
        "schema_version: 1\nconfig:\n  blueprint:\n    max_prs_per_epic: 0\n"
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    before = read_entries()
    # --max-prs omitted -> reads config, which is invalid -> structured error.
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])
    assert result.exit_code != 0
    assert "max_prs_per_epic" in result.output
    assert read_entries() == before
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]


def test_redecompose_clearing_waves_resets_details(graph_env):
    """Re-decompose with waves cleared must not leave stale details (codex P2)."""
    g, read_entries = graph_env
    one = [{"slug": "1", "title": "G1", "waves": "1-3", "blocked_by_groups": []}]
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(one)])
    g1 = _child(read_entries(), "1")
    assert "1-3" in (g1.get("details") or "")

    cleared = [{"slug": "1", "title": "G1", "waves": "", "blocked_by_groups": []}]
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(cleared)])
    g1 = _child(read_entries(), "1")
    assert not (g1.get("details") or ""), f"stale details: {g1.get('details')!r}"


# -- ab-9e864e42: decompose records expected_url_count on the shared doc --


@pytest.fixture
def graph_env_real_doc(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, request: pytest.FixtureRequest
):
    """Like graph_env but the epic's plan_path points at a real doc on disk.

    Returns (graph_path, read_entries, doc_path) so a test can assert that
    decompose stamps expected_url_count onto the shared design doc.

    The decompose -> set-expected path now runs the in-package
    ``fno.plan._stamp`` module via ``python3 -m``, which resolves regardless of
    cwd, so no FNO_REPO_ROOT pinning is needed to locate it.
    """
    import fno.graph._constants as gc
    import fno.graph.store as gs

    doc = tmp_path / "big.md"
    doc.write_text("---\ntitle: Big epic\nstatus: draft\n---\n# body\n")

    g = tmp_path / "graph.json"
    epic = _node(
        "ab-epic0001",
        title="Epic: big thing",
        plan_path=f"{doc}#c1-anchor",
        priority="p1",
        status="ready",
    )
    g.write_text(json.dumps({"entries": [epic]}) + "\n")

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)

    def read_entries():
        return json.loads(g.read_text())["entries"]

    return g, read_entries, doc


def test_decompose_writes_expected_url_count(graph_env_real_doc):
    """AC0-HP: decompose stamps expected_url_count = number of groups on the doc."""
    g, read_entries, doc = graph_env_real_doc
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)]
    )
    assert result.exit_code == 0, result.output
    assert "expected_url_count: 3" in doc.read_text()


def test_decompose_redecompose_updates_expected_url_count(graph_env_real_doc):
    """AC2-FR: re-decomposing to a different group count overwrites the doc's count."""
    g, read_entries, doc = graph_env_real_doc
    two = THREE_GROUPS[:2]
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(two)])
    assert "expected_url_count: 2" in doc.read_text()

    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])
    text = doc.read_text()
    assert "expected_url_count: 3" in text
    assert "expected_url_count: 2" not in text


def test_decompose_missing_doc_is_benign(graph_env, tmp_path):
    """A missing base doc must not fail decompose (it can never graduate early)."""
    (tmp_path / "big.md").unlink()  # drop the fixture doc: base is now missing
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)]
    )
    assert result.exit_code == 0, result.output


def _wire_graph(tmp_path, monkeypatch, epic):
    """Wire a one-epic graph.json into the CLI. The set-expected path runs the
    in-package ``fno.plan._stamp`` module via ``python3 -m`` (resolves regardless
    of cwd), so no FNO_REPO_ROOT pinning is needed. Returns (graph_path,
    read_entries)."""
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", tmp_path / "graph.json")
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gs, "GRAPH_JSON", tmp_path / "graph.json")
    (tmp_path / "graph.json").write_text(json.dumps({"entries": [epic]}) + "\n")

    def read_entries():
        return json.loads((tmp_path / "graph.json").read_text())["entries"]

    return tmp_path / "graph.json", read_entries


def test_decompose_resolves_relative_plan_path_against_cwd(tmp_path, monkeypatch):
    """A relative epic plan_path resolves against the epic's cwd, not the process
    cwd, so decompose still writes the count (Codex P1: avoid false 'missing')."""
    proj = tmp_path / "proj"
    (proj / "plans").mkdir(parents=True)
    doc = proj / "plans" / "big.md"
    doc.write_text("---\nstatus: draft\n---\n# body\n")

    epic = _node(
        "ab-epic0001",
        title="Epic",
        plan_path="plans/big.md#anchor",  # relative
        cwd=str(proj),
    )
    _wire_graph(tmp_path, monkeypatch, epic)
    monkeypatch.chdir(tmp_path)  # process cwd != epic cwd

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS[:2])]
    )
    assert result.exit_code == 0, result.output
    assert "expected_url_count: 2" in doc.read_text()


def test_decompose_malformed_doc_warns_but_exits_zero(tmp_path, monkeypatch):
    """A doc that exists but has malformed frontmatter: warn loudly, but decompose
    still exits 0 (Codex P1: never hard-fail decompose on a best-effort stamp)."""
    doc = tmp_path / "big.md"
    # Indented line at top level -> the parser's nested-structure error.
    doc.write_text("---\nstatus: draft\n  stray: nested\n---\n# body\n")

    epic = _node("ab-epic0001", title="Epic", plan_path=f"{doc}#anchor", cwd=str(tmp_path))
    g, read_entries = _wire_graph(tmp_path, monkeypatch, epic)

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)]
    )
    assert result.exit_code == 0, result.output
    assert "could not record expected_url_count" in result.output
    # Groups were still created despite the stamp warning.
    assert len([e for e in read_entries() if e.get("parent") == "ab-epic0001"]) == 3


def test_set_expected_count_spawn_failure_is_failed_not_skipped(tmp_path, monkeypatch):
    """A spawn failure is indeterminate, so it maps to 'failed' (surfaced), not a
    silent 'skipped' (an absent doc would prove no early-graduation risk; a spawn
    error does not). The stamp module runs via ``python3 -m`` so no repo-root
    resolution precedes the subprocess.run we stub to raise."""
    import fno.graph.cli as gcli
    import subprocess

    def _boom(*a, **k):
        raise OSError("permission denied")

    monkeypatch.setattr(subprocess, "run", _boom)

    status, detail = gcli._set_expected_count("/some/doc.md", 3)
    assert status == "failed"
    assert "spawn failed" in detail


# -- G2: hard|contract dependency classification --

from fno.graph._decompose import (  # noqa: E402
    STUB_MARKERS,
    DecomposeError,
    canonical_child_plan_path,
    classify_group_dep,
    extract_contract_versions,
    extract_why_digest,
    scaffold_separate_plan,
    separate_plan_path,
    validate_groups,
)


def _canonical(child: dict) -> Path:
    """The canonical scaffold path for a child node, computed the way the code does
    (child_root == the child's own cwd: routed cwd or inherited epic cwd)."""
    return Path(
        canonical_child_plan_path(
            child["group_slug"], child["id"], child["cwd"], child.get("created_at")
        )
    )

_CONTRACT_BODY = (
    "## Interface Contract\n\n"
    "**contract_version: 2**\n\n"
    "- `POST /api/widgets` -> `{ id: string }`\n"
)


def _contract_doc(tmp_path: Path, body: str = _CONTRACT_BODY) -> Path:
    doc = tmp_path / "big.md"
    doc.write_text(f"---\ntitle: Big epic\nstatus: draft\n---\n# body\n\n{body}\n")
    return doc


# extract_contract_versions (pure) --------------------------------------------


def test_extract_contract_versions_single():
    assert extract_contract_versions(_CONTRACT_BODY) == {2}


def test_extract_contract_versions_multi():
    text = (
        "## Interface Contract\n\n"
        "### Contract v3\n...\n### Contract v2\n...\n"
    )
    assert extract_contract_versions(text) == {2, 3}


def test_extract_contract_versions_no_heading_is_empty():
    # A stray version marker with no `## Interface Contract` heading is not a pin.
    assert extract_contract_versions("**contract_version: 1**") == set()


def test_extract_contract_versions_ignores_stray_outside_section():
    # Version markers outside the Interface Contract section body do not satisfy
    # the pin gate (gemini HIGH / codex P2): only the v2 inside the section counts.
    text = (
        "## Discussion\n"
        "We should upgrade from **contract_version: 1** to 2.\n\n"
        "## Interface Contract\n"
        "**contract_version: 2**\n\n"
        "## Next Section\n"
        "**contract_version: 3**\n"
    )
    assert extract_contract_versions(text) == {2}


def _grp(slug="1", title="Group 1: foundation", waves="1-2"):
    return validate_groups([{"slug": slug, "title": title, "waves": waves}], None)[0]


# canonical_child_plan_path (pure) --------------------------------------------


def test_canonical_child_plan_path_shape_and_routing():
    from fno.graph._decompose import canonical_child_plan_path

    p = canonical_child_plan_path(
        "etl-search", "x-abcd", "/repos/web", "2026-03-04T00:00:00+00:00"
    )
    # Filename is the `fno do plan path` shape with the child's created_at date...
    assert Path(p).name == "20260304-etl-search-x-abcd.md"
    # ...routed under the CHILD root's plans dir, not the epic's.
    assert p.startswith("/repos/web/")


def test_canonical_child_plan_path_corrupt_created_at_degrades(capsys):
    import datetime

    from fno.graph._decompose import canonical_child_plan_path

    # AC2-FR: an unparseable created_at falls back to today + a stderr warning,
    # never raises.
    p = canonical_child_plan_path("etl", "x-dead", "/repos/web", "not-a-date")
    today = datetime.datetime.now().strftime("%Y%m%d")
    assert Path(p).name == f"{today}-etl-x-dead.md"
    assert "created_at" in capsys.readouterr().err


# scaffold_separate_plan shape (US1 stub-proof + US4 why) ----------------------


def test_scaffold_born_undesigned_not_ready():
    # US1: the scaffold is born at a pre-design rung, never `ready` - the whole
    # point. The rung is spelled `idea` now; `stub` was the same rung under a
    # word no status vocabulary knew.
    text = scaffold_separate_plan(_grp(), "ab-epic0001", "big.md", why_digest="the why")
    assert "status: idea\n" in text
    assert "status: ready" not in text


def test_scaffold_carries_stub_markers_for_unfilled_sections():
    text = scaffold_separate_plan(_grp(), "ab-epic0001", "big.md", why_digest="the why")
    assert any(m in text for m in STUB_MARKERS)
    assert "## Changes" in text and "## Files to Modify" in text and "## Verification" in text


def test_scaffold_seeds_why_section_from_digest():
    # US4: a real digest is transcribed into ## Why (from epic), not a marker.
    text = scaffold_separate_plan(_grp(), "ab-epic0001", "big.md", why_digest="ground the tasks")
    assert "## Why (from epic)" in text
    assert "ground the tasks" in text
    assert "<!-- Why (from epic):" not in text  # non-empty why is not a stub


def test_scaffold_empty_why_falls_back_to_stub_marker():
    # US4 fallback: no digest -> the ## Why section carries the empty-why sentinel,
    # itself a stub marker the validator rejects.
    text = scaffold_separate_plan(_grp(), "ab-epic0001", "big.md", why_digest="")
    assert "<!-- Why (from epic):" in text


def test_scaffold_seeds_adopted_coverage_checklist():
    # AC8 (x-d9a4 task 1.7): a group that folds three existing nodes seeds them
    # as a coverage checklist - node-level coverage is mechanical, but nothing
    # verifies a folded group's plan addresses every commitment it absorbed,
    # because both close on merge either way.
    adopted = [
        ("ab-00000001", "Add validation"),
        ("ab-00000002", "Log the retry"),
        ("ab-00000003", "Doc the flag"),
    ]
    text = scaffold_separate_plan(
        _grp(), "ab-epic0001", "big.md", why_digest="w", adopted=adopted
    )
    assert "## Adopted coverage" in text
    # Sits between Context and Changes, so a reader hits it before the work.
    assert text.index("## Context") < text.index("## Adopted coverage") < text.index("## Changes")
    for nid, title in adopted:
        assert f"- [ ] `{nid}` - {title}" in text


def test_scaffold_with_no_adopt_is_byte_identical_to_mint_only():
    # AC10 (x-d9a4 task 1.7): an empty/absent adopt list must not change the
    # scaffold at all. Assert the bytes across default/None/[], not merely the
    # absence of a heading.
    base = scaffold_separate_plan(_grp(), "ab-epic0001", "big.md", why_digest="w")
    none_adopt = scaffold_separate_plan(
        _grp(), "ab-epic0001", "big.md", why_digest="w", adopted=None
    )
    empty_adopt = scaffold_separate_plan(
        _grp(), "ab-epic0001", "big.md", why_digest="w", adopted=[]
    )
    assert base == none_adopt == empty_adopt
    assert "Adopted coverage" not in base


# extract_why_digest (US4) ----------------------------------------------------


_EPIC_WITH_LOCKED = (
    "---\ntitle: E\n---\n\n"
    "# Epic\n\n"
    "## Overview\n\n"
    "The dispatcher stampedes thin stub plans; gate launch on a real plan.\n\n"
    "More overview prose that should not leak into the intent line.\n\n"
    "## Architecture\n\nstuff\n\n"
    "## Locked Decisions (DO NOT revisit)\n\n"
    "1. Inline-fill is mandatory.\n2. Fan-out is flag-scoped.\n"
)


def test_extract_why_digest_intent_plus_locked():
    digest, warning = extract_why_digest(_EPIC_WITH_LOCKED)
    assert warning is None
    assert "The dispatcher stampedes thin stub plans" in digest
    assert "More overview prose" not in digest  # only the first paragraph
    assert "Inline-fill is mandatory" in digest
    assert "Fan-out is flag-scoped" in digest


def test_extract_why_digest_no_locked_degrades_with_warning():
    doc = "## Overview\n\nJust the intent, no locked block.\n\n## Architecture\n\nx\n"
    digest, warning = extract_why_digest(doc)
    assert "Just the intent" in digest
    assert warning is not None and "Locked Decisions" in warning


# needs_think validation + flagged fan-out (US3) ------------------------------


def test_validate_needs_think_defaults_false():
    assert validate_groups([{"slug": "1", "title": "g"}], None)[0]["needs_think"] is False


def test_validate_needs_think_accepts_bool():
    norm = validate_groups([{"slug": "1", "title": "g", "needs_think": True}], None)
    assert norm[0]["needs_think"] is True


def test_validate_needs_think_rejects_non_bool():
    import pytest as _pytest

    with _pytest.raises(DecomposeError):
        validate_groups([{"slug": "1", "title": "g", "needs_think": "yes"}], None)


def _spy_spawn(monkeypatch):
    """Patch both spawn seams; return (fanout_calls, born_calls). Each fan-out
    call records a kwargs dict and returns a `spawned` result by default.

    `on_node_born` is patched to prove decompose never reaches it: the
    unflagged lane was deleted, so `needs_think` is the sole consent."""
    import fno.provenance.spawn_think as st

    fanout: list = []
    offers: list = []

    def fake_maybe(node, *, run_state=None, env=None, quiet=False, **k):
        fanout.append({"id": (node or {}).get("id"), "env": env or {}, **k})
        return st.ThinkSpawnResult("spawned", st.EVENT_SPAWNED, node_id=node.get("id"))

    def fake_born(node, *, run_state=None, **k):
        offers.append((node or {}).get("id"))

    monkeypatch.setattr(st, "maybe_spawn_think", fake_maybe)
    monkeypatch.setattr(st, "on_node_born", fake_born)
    return fanout, offers


def test_flagged_group_forces_fanout_unflagged_silent(graph_env, monkeypatch, tmp_path):
    """AC3-HP + AC4-HP: only a `needs_think` group reaches a spawn lane."""
    g, read_entries = graph_env
    fanout, born = _spy_spawn(monkeypatch)
    groups = [
        {"slug": "1", "title": "G1 spike", "waves": "1", "blocked_by_groups": [],
         "needs_think": True},
        {"slug": "2", "title": "G2 rote", "waves": "2", "blocked_by_groups": ["1"]},
    ]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output

    # The flagged child took the fan-out lane with the gate + spawn forced on;
    # the unflagged child reached no spawn seam at all.
    assert len(fanout) == 1 and born == []
    call = fanout[0]
    assert call["env"].get("FNO_THINK_SPAWN") == "1"
    assert call["env"].get("FNO_THINK_SPAWN_ATTENDED") == "spawn"
    # x-edf7 review fixes: fan-out chains blueprint, threads the why-digest (its
    # content depends on the epic doc - extraction is covered separately), and
    # scopes the /think doc to the CHILD's repo (project_root == child cwd).
    assert call["chain_blueprint"] is True
    assert "why_digest" in call
    assert str(call["project_root"]) == str(tmp_path)  # graph_env epic cwd


def test_fanout_project_root_is_child_repo_for_cross_repo(graph_env, monkeypatch):
    # P1: a needs_think child routed into a foreign repo resolves its /think doc
    # from THAT repo, not the epic's - project_root must be the child cwd.
    g, read_entries = graph_env
    fanout, _ = _spy_spawn(monkeypatch)
    _patch_workmap(monkeypatch, {"web": "/repos/web"})
    groups = [{"slug": "1", "title": "G1 web spike", "waves": "1",
               "blocked_by_groups": [], "needs_think": True, "project": "web"}]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output
    assert str(fanout[0]["project_root"]) == "/repos/web"


def test_fanout_non_spawn_prints_offer_fallback(graph_env, monkeypatch):
    import fno.provenance.spawn_think as st

    g, read_entries = graph_env

    def fake_maybe(node, *, run_state=None, env=None, quiet=False, **k):
        # Simulate a cap/failure: no spawn.
        return st.ThinkSpawnResult("skipped", st.EVENT_SKIPPED, reason="cap-exceeded",
                                   node_id=node.get("id"))

    monkeypatch.setattr(st, "maybe_spawn_think", fake_maybe)
    groups = [{"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": [],
               "needs_think": True}]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output
    # AC2-ERR / AC1-EDGE: a child that did not spawn is left idea + an OFFER line.
    assert "did not spawn" in result.output and "/think" in result.output
    child = _child(read_entries(), "1")
    assert child["plan_path"] is None and child["status"] == "idea"


def test_fanout_raise_never_wedges_committed_mutation(graph_env, monkeypatch):
    """AC5-ERR: a raising spawn leaves the already-committed graph mutation intact.

    Step 4a runs after the graph write commits, so its `except Exception` wrapper
    is what keeps a spawn crash from losing the decompose. Pinned here because
    this change edits this block.
    """
    import fno.provenance.spawn_think as st

    g, read_entries = graph_env

    def boom(node, *, run_state=None, env=None, quiet=False, **k):
        raise RuntimeError("spawn substrate unavailable")

    monkeypatch.setattr(st, "maybe_spawn_think", boom)
    groups = [{"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": [],
               "needs_think": True}]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output
    assert _child(read_entries(), "1") is not None  # the child survived


def test_json_mode_reports_fanout_outcome(graph_env, monkeypatch):
    # P2: a machine caller must see when a flagged child was left an unlinked idea
    # (the fallback stderr line is suppressed under --json), so the outcome rides
    # in the JSON payload.
    import fno.provenance.spawn_think as st

    g, read_entries = graph_env

    def fake_maybe(node, *, run_state=None, env=None, quiet=False, **k):
        return st.ThinkSpawnResult("skipped", st.EVENT_SKIPPED, reason="cap-exceeded",
                                   node_id=node.get("id"))

    monkeypatch.setattr(st, "maybe_spawn_think", fake_maybe)
    groups = [{"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": [],
               "needs_think": True}]
    result = _invoke(["backlog", "--json", "decompose", "ab-epic0001",
                      "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert len(payload["fanout"]) == 1
    assert payload["fanout"][0]["decision"] == "skipped"
    assert payload["fanout"][0]["reason"] == "cap-exceeded"


def test_all_unflagged_spawns_nothing_even_with_gate_on(graph_env, monkeypatch):
    """AC3-HP: the reported case - gate enabled, autonomous session, no flags.

    This is exactly the reported repro: `think_spawn.enabled: true` is the common
    install, and an autonomous decompose always classifies presence `away`, so
    the old unflagged lane's "offer" spawned instead of offering.
    """
    g, read_entries = graph_env
    fanout, born = _spy_spawn(monkeypatch)
    monkeypatch.setenv("FNO_THINK_SPAWN", "1")
    monkeypatch.setenv("FNO_THINK_SPAWN_PRESENCE", "away")
    groups = [
        {"slug": "1", "title": "G1 rote", "waves": "1", "blocked_by_groups": []},
        {"slug": "2", "title": "G2 rote", "waves": "2", "blocked_by_groups": ["1"]},
    ]
    result = _invoke(["backlog", "--json", "decompose", "ab-epic0001",
                      "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output
    assert fanout == [] and born == []
    assert json.loads(result.output)["fanout"] == []


def test_redecompose_reattempts_unlinked_flagged_skips_linked(graph_env, monkeypatch):
    g, read_entries = graph_env
    fanout, _ = _spy_spawn(monkeypatch)
    groups = [{"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": [],
               "needs_think": True}]
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(groups)])
    assert len(fanout) == 1  # first pass fires the fan-out

    # Child still unlinked (spawn is fire-and-forget) -> re-decompose re-attempts.
    fanout.clear()
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(groups)])
    assert len(fanout) == 1  # AC1-FR: unlinked flagged child re-designed

    # Once linked (designed), re-decompose leaves it alone.
    fanout.clear()
    entries = read_entries()
    _child(entries, "1")["plan_path"] = "/plans/big.group-1.md"
    Path(g).write_text(json.dumps({"entries": entries}) + "\n")
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(groups)])
    assert fanout == []  # linked child is not re-designed


def test_extract_why_digest_no_overview_uses_first_prose_paragraph():
    doc = "---\ntitle: E\n---\n\n# Heading\n\nFirst real prose paragraph is the intent.\n"
    digest, _ = extract_why_digest(doc)
    assert "First real prose paragraph" in digest


def test_extract_why_digest_empty_doc_is_empty():
    assert extract_why_digest("") == ("", None)


def test_extract_why_digest_overview_with_suffix_and_crlf():
    # Robustness: `## Overview: <suffix>` matches, and CRLF is normalized.
    doc = "## Overview: the goal\r\n\r\nThe intent line here.\r\n\r\n## Next\r\nx\r\n"
    digest, _ = extract_why_digest(doc)
    assert "The intent line here." in digest
    assert "\r" not in digest


def test_extract_contract_versions_empty_doc():
    assert extract_contract_versions("") == set()


# classify_group_dep (pure) ---------------------------------------------------


def test_classify_hard_is_default():
    grp = {"slug": "1", "dep": "hard", "stub_against": None}
    assert classify_group_dep(grp, {1, 2}, "doc.md") == ("hard", None, None, None)


def test_classify_contract_with_pin_uses_newest_version():
    grp = {"slug": "2", "dep": "contract", "stub_against": None}
    dep, stub, ver, downgrade = classify_group_dep(grp, {1, 2}, "doc.md")
    assert (dep, ver, downgrade) == ("contract", 2, None)
    assert stub == "doc.md#interface-contract"


def test_classify_contract_explicit_stub_against_override():
    grp = {"slug": "2", "dep": "contract", "stub_against": "other.md#api-v1"}
    _, stub, _, _ = classify_group_dep(grp, {1}, "doc.md")
    assert stub == "other.md#api-v1"


def test_classify_contract_no_pin_downgrades_to_hard():
    grp = {"slug": "2", "dep": "contract", "stub_against": None}
    dep, stub, ver, downgrade = classify_group_dep(grp, set(), "doc.md")
    assert (dep, stub, ver) == ("hard", None, None)
    assert downgrade and "falling back to hard" in downgrade


# validate_groups (pure) ------------------------------------------------------


def test_validate_rejects_unknown_dep_tier():
    with pytest.raises(DecomposeError):
        validate_groups([{"slug": "1", "title": "g", "dep": "soft"}], None)


def test_validate_rejects_contract_without_blocker():
    spec = [{"slug": "1", "title": "g", "dep": "contract", "blocked_by_groups": []}]
    with pytest.raises(DecomposeError, match="must name its blocker"):
        validate_groups(spec, None)


def test_validate_rejects_empty_stub_against():
    spec = [{"slug": "1", "title": "g", "stub_against": "   "}]
    with pytest.raises(DecomposeError):
        validate_groups(spec, None)


def test_validate_defaults_dep_to_hard():
    norm = validate_groups([{"slug": "1", "title": "g"}], None)
    assert norm[0]["dep"] == "hard"
    assert norm[0]["stub_against"] is None


# CLI integration -------------------------------------------------------------


def _contract_env(tmp_path, monkeypatch, body=_CONTRACT_BODY):
    doc = _contract_doc(tmp_path, body)
    epic = _node("ab-epic0001", title="Epic", plan_path=f"{doc}#anchor", cwd=str(tmp_path))
    return _wire_graph(tmp_path, monkeypatch, epic) + (doc,)


_CONTRACT_GROUPS = [
    {"slug": "1", "title": "Group 1: backend", "waves": "1-2", "blocked_by_groups": []},
    {
        "slug": "2",
        "title": "Group 2: frontend",
        "waves": "3-4",
        "blocked_by_groups": ["1"],
        "dep": "contract",
    },
]


def test_ac2_hp_contract_with_pin_stamps_child(tmp_path, monkeypatch):
    g, read_entries, doc = _contract_env(tmp_path, monkeypatch)
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(_CONTRACT_GROUPS)]
    )
    assert result.exit_code == 0, result.output

    child = _child(read_entries(), "2")
    assert child["dep"] == "contract"
    assert child["contract_version"] == 2
    assert child["stub_against"] == f"{doc}#interface-contract"
    # The hard sibling carries none of the contract fields (AC6-EDGE).
    sib = _child(read_entries(), "1")
    assert "dep" not in sib and "stub_against" not in sib and "contract_version" not in sib


def test_ac2_hp_contract_no_pin_downgrades_loudly(tmp_path, monkeypatch):
    # Doc has frontmatter but no ## Interface Contract section -> no pin.
    g, read_entries, doc = _contract_env(tmp_path, monkeypatch, body="# just a body\n")
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(_CONTRACT_GROUPS)]
    )
    assert result.exit_code == 0, result.output
    assert "falling back to hard" in result.output

    child = _child(read_entries(), "2")
    assert "dep" not in child
    assert "contract_version" not in child


def test_contract_downgrade_in_json_output(tmp_path, monkeypatch):
    g, read_entries, doc = _contract_env(tmp_path, monkeypatch, body="# no contract\n")
    result = _invoke(
        ["--json", "backlog", "decompose", "ab-epic0001",
         "--groups", _groups_json(_CONTRACT_GROUPS)]
    )
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert len(payload["downgrades"]) == 1


def test_redecompose_contract_to_hard_clears_stub_fields(tmp_path, monkeypatch):
    g, read_entries, doc = _contract_env(tmp_path, monkeypatch)
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(_CONTRACT_GROUPS)])
    child = _child(read_entries(), "2")
    assert child["dep"] == "contract"

    # Re-decompose group 2 back to hard (drop the dep field).
    hard_again = [
        _CONTRACT_GROUPS[0],
        {**_CONTRACT_GROUPS[1], "dep": "hard"},
    ]
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(hard_again)])
    child = _child(read_entries(), "2")
    assert "dep" not in child
    assert "stub_against" not in child
    assert "contract_version" not in child


def test_contract_non_utf8_doc_does_not_crash(tmp_path, monkeypatch):
    """A non-UTF-8 epic doc must not hard-fail decompose; treat it as no pin."""
    doc = tmp_path / "big.md"
    doc.write_bytes(b"\xff\xfe## Interface Contract\n**contract_version: 1**\n")
    epic = _node("ab-epic0001", title="Epic", plan_path=f"{doc}#anchor", cwd=str(tmp_path))
    g, read_entries = _wire_graph(tmp_path, monkeypatch, epic)

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(_CONTRACT_GROUPS)]
    )
    assert result.exit_code == 0, result.output
    child = _child(read_entries(), "2")
    assert "dep" not in child  # unreadable doc -> no pin -> hard


def test_ac6_edge_pure_hard_decompose_adds_no_contract_fields(graph_env):
    """A decomposition with only hard deps stamps no contract metadata."""
    g, read_entries = graph_env
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])
    for child in (e for e in read_entries() if e.get("parent") == "ab-epic0001"):
        assert "dep" not in child
        assert "stub_against" not in child
        assert "contract_version" not in child


# -- packaging: separate is the only mode; legacy fragment is recognized, not authored --


def _separate_env(tmp_path, monkeypatch):
    """A one-epic graph whose plan_path points at a real doc under tmp_path, so
    separate-mode scaffolds land inside the test's tmp dir. Returns (read_entries, doc)."""
    doc = tmp_path / "epic.md"
    doc.write_text("---\nstatus: ready\nscope: epic\n---\n# Epic\n", encoding="utf-8")
    epic = _node(
        "ab-epic0001",
        title="Epic",
        plan_path=f"{doc}#anchor",
        cwd=str(tmp_path),
        status="ready",
    )
    _, read_entries = _wire_graph(tmp_path, monkeypatch, epic)
    return read_entries, doc


def test_plans_separate_scaffolds_files_and_repoints(tmp_path, monkeypatch):
    """--plans separate writes a self-contained quick-plan per child and repoints
    each child's plan_path to that file (Change 2 / Verification 2)."""
    read_entries, doc = _separate_env(tmp_path, monkeypatch)
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--plans", "separate",
         "--groups", _groups_json(THREE_GROUPS)]
    )
    assert result.exit_code == 0, result.output

    children = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    assert len(children) == 3
    for c in children:
        # x-edf7 US2: the scaffold FILE is still written, but the node is born
        # UNLINKED - plan_path stays None until inline-fill links the filled plan.
        assert c["plan_path"] is None
        # x-d6a6: born at the canonical `fno do plan path` name, not the legacy
        # `.group-<slug>.md`. The legacy path is no longer written.
        f = _canonical(c)
        assert f.exists(), f"scaffold not written: {f}"
        assert f.name.endswith(f"-{c['group_slug']}-{c['id']}.md")
        assert not Path(separate_plan_path(str(doc), c["group_slug"])).exists()
        body = f.read_text()
        assert "status: idea" in body       # born undesigned, never ready
        assert "kind: quick-plan" in body
        assert "parent_epic: ab-epic0001" in body


def test_plans_separate_idempotent_preserves_builder_edits(tmp_path, monkeypatch):
    """Re-running separate mode upserts (no dupes) and never clobbers a file a
    builder has already edited (Concurrency invariant)."""
    read_entries, doc = _separate_env(tmp_path, monkeypatch)
    _invoke(["backlog", "decompose", "ab-epic0001", "--plans", "separate",
             "--groups", _groups_json(THREE_GROUPS)])
    before = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    ids_before = sorted(e["id"] for e in before)
    # Children are born unlinked (plan_path=None); the scaffold file lives at the
    # slug-derived path, so a builder edits THAT file, not a linked plan_path.
    edited = Path(separate_plan_path(str(doc), before[0]["group_slug"]))
    edited.write_text("# builder edits - keep\n", encoding="utf-8")

    _invoke(["backlog", "decompose", "ab-epic0001", "--plans", "separate",
             "--groups", _groups_json(THREE_GROUPS)])
    after = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    assert sorted(e["id"] for e in after) == ids_before   # no duplicates
    assert edited.read_text() == "# builder edits - keep\n"  # not overwritten


def test_legacy_fragment_children_repointed_to_separate(tmp_path, monkeypatch):
    """A pre-removal epic whose children still carry the legacy #group- fragment
    plan_path is repointed to its own .group-<slug>.md on re-decompose, upserting
    the SAME children (idempotent on the slug across both forms - the migration
    path)."""
    read_entries, doc = _separate_env(tmp_path, monkeypatch)
    g = doc.parent / "graph.json"
    base = str(doc)
    # Seed two legacy fragment children as an old (pre-removal) decompose left them.
    entries = read_entries()
    for slug in ("1", "2"):
        entries.append(
            _node(f"ab-frag000{slug}", parent="ab-epic0001",
                  plan_path=f"{base}#group-{slug}")
        )
    g.write_text(json.dumps({"entries": entries}) + "\n")
    frag_ids = sorted(e["id"] for e in read_entries() if e.get("parent") == "ab-epic0001")

    two = [
        {"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []},
        {"slug": "2", "title": "G2", "waves": "2", "blocked_by_groups": ["1"]},
    ]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(two)])
    assert result.exit_code == 0, result.output
    sep = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    assert sorted(e["id"] for e in sep) == frag_ids   # same nodes, no dupes
    for c in sep:
        # The plan_path is repointed to the .md form (metadata migration)...
        assert "#group-" not in c["plan_path"]
        assert c["plan_path"].endswith(".md")
        # ...but x-d6a6 skip-if-linked means a linked child is NOT re-scaffolded:
        # no stub is spuriously minted (no migration; Locked Decision 4/6).
        assert not _canonical(c).exists()


def test_plans_fragment_rejected_with_removed_message(graph_env):
    """--plans fragment was removed; it errors with a pointer to separate,
    writing nothing (separate is the only packaging - one plan == one PR)."""
    g, read_entries = graph_env
    before = read_entries()
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--plans", "fragment",
                      "--groups", _groups_json(THREE_GROUPS)])
    assert result.exit_code != 0
    assert "removed" in result.output.lower() and "separate" in result.output.lower()
    assert read_entries() == before


def test_plans_separate_title_with_quotes_emits_valid_yaml(tmp_path, monkeypatch):
    """A group title containing a double quote must not break the scaffold's YAML
    frontmatter (gemini review: escape quotes)."""
    import yaml

    read_entries, doc = _separate_env(tmp_path, monkeypatch)
    groups = [{"slug": "1", "title": 'Group "alpha": the \\ case', "waves": "1",
               "blocked_by_groups": []}]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--plans", "separate",
                      "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output
    child = next(e for e in read_entries() if e.get("parent") == "ab-epic0001")
    body = _canonical(child).read_text()
    front = body.split("---\n", 2)[1]
    fm = yaml.safe_load(front)
    assert fm["title"] == 'Group "alpha": the \\ case'


def test_plans_invalid_value_rejected_atomically(graph_env):
    """An unknown --plans value errors before any graph write."""
    g, read_entries = graph_env
    before = read_entries()
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--plans", "bogus",
                      "--groups", _groups_json(THREE_GROUPS)])
    assert result.exit_code != 0
    assert "separate" in result.output.lower()
    assert read_entries() == before


# -- x-d6a6: canonical child plan names + per-project routing at birth --------


def test_ac1_hp_child_born_at_canonical_name_in_child_project_dir(tmp_path, monkeypatch):
    """AC1-HP: a routed child's stub lands at the canonical `fno do plan path` name in
    the CHILD project's plans dir, and the node stays born-unlinked."""
    read_entries, doc = _separate_env(tmp_path, monkeypatch)
    web_root = tmp_path / "web"
    web_root.mkdir()
    _patch_workmap(monkeypatch, {"web": str(web_root)})
    groups = [
        {"slug": "backend", "title": "G backend", "waves": "1", "blocked_by_groups": []},
        {"slug": "webui", "title": "G web", "waves": "2", "blocked_by_groups": ["backend"],
         "project": "web"},
    ]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--plans", "separate",
                      "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output
    children = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]

    web_child = _child(children, "webui")
    stub = _canonical(web_child)
    assert stub.exists(), f"stub not written: {stub}"
    # Routed under web's own plans dir, canonical name, still born-unlinked.
    assert str(stub).startswith(str(web_root))
    assert stub.name.endswith(f"-webui-{web_child['id']}.md")
    assert web_child["plan_path"] is None
    # The inherited backend child lands under the epic's root, not web's.
    assert not str(_canonical(_child(children, "backend"))).startswith(str(web_root))


def test_ac1_edge_redecompose_across_day_is_idempotent(tmp_path, monkeypatch):
    """AC1-EDGE: the canonical filename's date comes from created_at, not today, so
    a re-decompose on a later day recomputes the SAME path and skips the existing
    stub instead of minting a fresh-dated duplicate."""
    read_entries, doc = _separate_env(tmp_path, monkeypatch)
    g = doc.parent / "graph.json"
    entries = read_entries()
    child = _node("ab-child001", parent="ab-epic0001", group_slug="1",
                  cwd=str(tmp_path), created_at="2026-01-01T00:00:00+00:00",
                  plan_path=None)
    entries.append(child)
    g.write_text(json.dumps({"entries": entries}) + "\n")
    # The stub an earlier decompose left, dated from created_at (not today).
    stub = _canonical(child)
    stub.parent.mkdir(parents=True, exist_ok=True)
    stub.write_text("# earlier stub\n", encoding="utf-8")
    assert stub.name.startswith("20260101-")

    one = [{"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []}]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--plans", "separate",
                      "--groups", _groups_json(one)])
    assert result.exit_code == 0, result.output
    # No today-dated duplicate: exactly the one existing stub, unchanged.
    assert list(stub.parent.glob(f"*-1-{child['id']}.md")) == [stub]
    assert stub.read_text() == "# earlier stub\n"


def test_ac2_edge_legacy_group_file_grandfathered(tmp_path, monkeypatch):
    """AC2-EDGE: a child whose legacy `.group-<slug>.md` stub exists on disk is
    grandfathered - decompose leaves it in place and mints no canonical duplicate."""
    read_entries, doc = _separate_env(tmp_path, monkeypatch)
    g = doc.parent / "graph.json"
    entries = read_entries()
    child = _node("ab-child001", parent="ab-epic0001", group_slug="1",
                  cwd=str(tmp_path), plan_path=None)
    entries.append(child)
    g.write_text(json.dumps({"entries": entries}) + "\n")
    legacy = Path(separate_plan_path(str(doc), "1"))
    legacy.write_text("# legacy builder content - keep\n", encoding="utf-8")

    one = [{"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []}]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--plans", "separate",
                      "--groups", _groups_json(one)])
    assert result.exit_code == 0, result.output
    child_after = _child(read_entries(), "1")
    assert legacy.read_text() == "# legacy builder content - keep\n"  # untouched
    assert not _canonical(child_after).exists()  # no canonical duplicate


def test_redecompose_no_route_uses_persisted_child_cwd(tmp_path, monkeypatch):
    """A child already routed to another repo, re-decomposed with NO explicit
    route, scaffolds under its OWN persisted cwd - not the epic's - and mints no
    duplicate in the epic project (the child node's cwd is the authoritative root)."""
    read_entries, doc = _separate_env(tmp_path, monkeypatch)
    web_root = tmp_path / "web"
    web_root.mkdir()
    g = doc.parent / "graph.json"
    entries = read_entries()
    child = _node("ab-child001", parent="ab-epic0001", group_slug="1",
                  project="web", cwd=str(web_root), plan_path=None)
    entries.append(child)
    g.write_text(json.dumps({"entries": entries}) + "\n")

    one = [{"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []}]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--plans", "separate",
                      "--groups", _groups_json(one)])
    assert result.exit_code == 0, result.output
    child_after = _child(read_entries(), "1")
    assert child_after["cwd"] == str(web_root)          # repo untouched (no route)
    stub = _canonical(child_after)
    assert stub.exists()
    assert str(stub).startswith(str(web_root))          # under the child's own repo
    assert not str(stub).startswith(str(tmp_path / "internal"))  # not the epic dir


def test_ac3_edge_already_linked_child_not_rescaffolded(tmp_path, monkeypatch):
    """AC3-EDGE: a linked child (plan_path set) is skipped - no spurious stub is
    written beside its real plan, and the filled plan is never clobbered."""
    read_entries, doc = _separate_env(tmp_path, monkeypatch)
    g = doc.parent / "graph.json"
    filled = tmp_path / "filled-plan.md"
    filled.write_text("# real filled plan\n", encoding="utf-8")
    entries = read_entries()
    child = _node("ab-child001", parent="ab-epic0001", group_slug="1",
                  cwd=str(tmp_path), plan_path=str(filled))
    entries.append(child)
    g.write_text(json.dumps({"entries": entries}) + "\n")

    one = [{"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []}]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--plans", "separate",
                      "--groups", _groups_json(one)])
    assert result.exit_code == 0, result.output
    child_after = _child(read_entries(), "1")
    assert child_after["plan_path"] == str(filled)      # unchanged
    assert not _canonical(child_after).exists()          # no stub minted
    assert filled.read_text() == "# real filled plan\n"  # not clobbered


# wave-0 design fan-out (x-3571 wave 2) ---------------------------------------


def _wave0_on(monkeypatch, on: bool = True):
    """Arm/disarm the wave-0 lane through its env seam."""
    monkeypatch.setenv("FNO_THINK_SPAWN_WAVE0", "1" if on else "0")


def test_AC8_HP_wave0_fanout_off_by_default_is_byte_identical(
    graph_env, monkeypatch
):
    """No key, no env: only `needs_think` spawns, exactly as before."""
    monkeypatch.delenv("FNO_THINK_SPAWN_WAVE0", raising=False)
    fanout, born = _spy_spawn(monkeypatch)
    groups = [
        {"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []},
        {"slug": "2", "title": "G2", "waves": "2", "blocked_by_groups": ["1"]},
    ]
    result = _invoke(["backlog", "decompose", "ab-epic0001",
                      "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output
    assert fanout == [] and born == []


def test_AC7_HP_wave0_fanout_selects_wave0_children_only(graph_env, monkeypatch):
    """Wave 0 = no intra-epic blocker. G2 is blocked by G1, so it is wave 1."""
    g, read_entries = graph_env
    _wave0_on(monkeypatch)
    fanout, born = _spy_spawn(monkeypatch)
    groups = [
        {"slug": "1", "title": "G1 foundation", "waves": "1", "blocked_by_groups": []},
        {"slug": "2", "title": "G2 dependent", "waves": "2", "blocked_by_groups": ["1"]},
    ]
    result = _invoke(["backlog", "decompose", "ab-epic0001",
                      "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output

    # Assert the IDENTITY, not just the count: selecting exactly the WRONG
    # child (wave 1 instead of wave 0) also yields len == 1, so a cardinality
    # assertion passes a mutation that inverts the selector.
    assert len(fanout) == 1, f"expected only the wave-0 child, got {fanout}"
    picked = fanout[0]["id"]
    wave0_id = next(
        r["id"] for r in read_entries() if r.get("group_slug") == "1"
    )
    assert picked == wave0_id, f"fanned out the wrong child: {picked} != {wave0_id}"
    assert born == []


def test_wave0_fanout_covers_every_independent_child(graph_env, monkeypatch):
    """Two children with no blocker are both wave 0, so both fan out."""
    _wave0_on(monkeypatch)
    fanout, _ = _spy_spawn(monkeypatch)
    groups = [
        {"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []},
        {"slug": "2", "title": "G2", "waves": "1", "blocked_by_groups": []},
        {"slug": "3", "title": "G3", "waves": "2", "blocked_by_groups": ["1", "2"]},
    ]
    result = _invoke(["backlog", "decompose", "ab-epic0001",
                      "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output
    assert len(fanout) == 2


def test_wave0_lane_still_chains_blueprint(graph_env, monkeypatch):
    """A fanned-out child must be designed AND linked, not left at `design`."""
    _wave0_on(monkeypatch)
    fanout, _ = _spy_spawn(monkeypatch)
    groups = [{"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []}]
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(groups)])
    assert fanout[0]["chain_blueprint"] is True


def test_AC9_CON_ownership_is_keyed_on_the_spawn_receipt(graph_env, monkeypatch):
    """Exactly one lane may own a child, and a failed spawn owns nothing.

    Resolves Open Question 3: the fan-out claims a child only when its spawn
    actually fired. Predicting ownership from wave membership instead would
    orphan a child whose spawn failed - nobody would inline-fill it.
    """
    import fno.provenance.spawn_think as st
    _wave0_on(monkeypatch)

    def fake_maybe(node, *, run_state=None, env=None, quiet=False, **k):
        return st.ThinkSpawnResult("skipped", "think_skipped", node_id=node.get("id"))

    monkeypatch.setattr(st, "maybe_spawn_think", fake_maybe)
    groups = [{"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []}]
    result = _invoke(["backlog", "--json", "decompose", "ab-epic0001",
                      "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output

    payload = json.loads(result.output)
    entries = payload.get("fanout") or []
    assert entries, "a wave-0 attempt must still be reported"
    for e in entries:
        assert e["decision"] != "spawned"
        assert e["owned"] is False, "a failed spawn must leave the child to inline-fill"


def test_wave0_receipt_names_the_lane_and_claims_ownership(graph_env, monkeypatch):
    _wave0_on(monkeypatch)
    _spy_spawn(monkeypatch)
    groups = [{"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []}]
    result = _invoke(["backlog", "--json", "decompose", "ab-epic0001",
                      "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output
    entries = json.loads(result.output)["fanout"]
    assert entries[0]["lane"] == "wave0"
    assert entries[0]["owned"] is True


def test_wave0_spawn_failure_never_wedges_the_committed_mutation(
    graph_env, monkeypatch
):
    """The graph mutation already committed; a fan-out fault must not undo it."""
    import fno.provenance.spawn_think as st
    _wave0_on(monkeypatch)

    def boom(*a, **k):
        raise RuntimeError("spawn substrate unavailable")

    monkeypatch.setattr(st, "maybe_spawn_think", boom)
    groups = [{"slug": "1", "title": "G1", "waves": "1", "blocked_by_groups": []}]
    result = _invoke(["backlog", "decompose", "ab-epic0001",
                      "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output


def test_a_child_linked_to_an_idea_scaffold_is_still_a_design_candidate(
    graph_env, monkeypatch, tmp_path
):
    """`_needs_design` reads the rung, not `plan_path` presence.

    Every other fan-out test births children UNLINKED, so the rung check and the
    old `child.get("plan_path")` presence check are indistinguishable in them -
    swapping the implementation back leaves them all green. This constructs the
    case that separates them: a child that IS linked, to a doc still at the
    `idea` rung. Presence says "done, skip it"; the rung says "still undesigned".
    """
    from fno.graph.cli import _needs_design

    scaffold = tmp_path / "child-scaffold.md"
    scaffold.write_text("---\ntitle: G1\nstatus: idea\n---\n\n# G1\n")
    linked_child = {"id": "x-linked1", "plan_path": str(scaffold)}
    assert _needs_design(linked_child) is True, (
        "a linked-but-undesigned child must remain a design candidate"
    )

    filled = tmp_path / "child-filled.md"
    filled.write_text("---\ntitle: G1\nstatus: ready\n---\n\n# G1\n")
    assert _needs_design({"id": "x-linked2", "plan_path": str(filled)}) is False

    # ...and the unlinked case still holds, so the rung check is a superset.
    assert _needs_design({"id": "x-unlinked"}) is True


def test_starvation_receipt_names_a_linked_idea_child(graph_env, tmp_path):
    """A backlog of only undesigned children must not print a bare `null`.

    The persisted rung IS `idea` for the common linked-scaffold case, and
    `selection_guards` is gated on persisted `ready`, so without a dedicated arm
    the row falls through and is dropped from the receipt entirely.
    """
    from datetime import datetime, timezone

    from fno.graph.cli import _starvation_receipts

    scaffold = tmp_path / "s.md"
    scaffold.write_text("---\nstatus: idea\n---\n")
    node = {
        "id": "x-scaff01",
        "status": "idea",
        "plan_path": str(scaffold),
        "created_at": datetime.now(timezone.utc).isoformat(),
    }
    out = _starvation_receipts(
        [node], None, True, None, set(), datetime.now(timezone.utc), 21
    )
    assert out == [("x-scaff01", "idea")], f"receipt dropped the node: {out}"


# -- adopt: package existing epic children into groups (x-b9d7) --


def _epic_child(node_id: str, **overrides) -> dict:
    """A rich child born via `fno backlog idea --parent` - no group_slug."""
    return _node(node_id, parent="ab-epic0001", **{"status": "ready", **overrides})


def _seed_children(g, *children) -> None:
    entries = json.loads(g.read_text())["entries"]
    entries.extend(children)
    g.write_text(json.dumps({"entries": entries}) + "\n")


ADOPT_GROUP = [
    {"slug": "appendix", "title": "Appendix correctness", "waves": "1",
     "blocked_by_groups": [], "adopt": ["ab-kid00001", "ab-kid00002", "ab-kid00003"]},
]


def test_adopt_reparents_existing_children_minting_nothing(graph_env):
    """AC1: three hand-created children become the group's tasks, not duplicates."""
    g, read_entries = graph_env
    _seed_children(
        g,
        _epic_child("ab-kid00001", title="row counts", details="verified 4,182 rows"),
        _epic_child("ab-kid00002", title="related-party export", plan_path="/p/two.md"),
        _epic_child("ab-kid00003", title="appendix B", priority="p1"),
    )
    before = {e["id"] for e in read_entries()}

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(ADOPT_GROUP)]
    )
    assert result.exit_code == 0, result.output

    entries = read_entries()
    child = _child(entries, "appendix")
    by_id = {e["id"]: e for e in entries}

    # Exactly one node was minted: the group child itself.
    assert set(by_id) - before == {child["id"]}
    assert before <= set(by_id)  # nothing deleted

    for kid in ("ab-kid00001", "ab-kid00002", "ab-kid00003"):
        assert by_id[kid]["parent"] == child["id"]
        # group_slug is the identity key find_orphans and the upsert lookup use;
        # a second claimant for one slug would break re-decompose idempotency.
        assert by_id[kid].get("group_slug") is None

    # The epic keeps exactly one direct child now: the group.
    assert [e["id"] for e in entries if e.get("parent") == "ab-epic0001"] == [child["id"]]

    # Evidence-carrying fields are untouched by adoption.
    assert by_id["ab-kid00001"]["details"] == "verified 4,182 rows"
    assert by_id["ab-kid00002"]["plan_path"] == "/p/two.md"
    assert by_id["ab-kid00003"]["priority"] == "p1"


def test_adopt_reports_adopted_ids(graph_env):
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"), _epic_child("ab-kid00002"),
                   _epic_child("ab-kid00003"))
    result = _invoke(
        ["--json", "backlog", "decompose", "ab-epic0001",
         "--groups", _groups_json(ADOPT_GROUP)]
    )
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["groups"][0]["adopted"] == [
        "ab-kid00001", "ab-kid00002", "ab-kid00003"
    ]


def test_adopt_rerun_is_an_idempotent_noop(graph_env):
    """AC2: re-running the same spec re-parents nothing and settles the bytes.

    Byte identity is asserted between runs 2 and 3, not 1 and 2. A plain
    re-decompose carrying no `adopt` key is already not byte-identical on its
    FIRST re-run: ``read_graph`` migrates entries on read, so a node minted in
    run 1 gains its full default field set the next time it is written back.
    The graph converges at run 2 and is stable from there. Pinning run 1 == run
    2 would fail on today's mint path with no adopt list in sight.
    """
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"), _epic_child("ab-kid00002"),
                   _epic_child("ab-kid00003"))
    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(ADOPT_GROUP)]
    ).exit_code == 0
    ids_after_first = {e["id"] for e in read_entries()}
    parents_after_first = {e["id"]: e.get("parent") for e in read_entries()}

    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(ADOPT_GROUP)]
    ).exit_code == 0
    settled = g.read_text()

    result = _invoke(
        ["--json", "backlog", "decompose", "ab-epic0001",
         "--groups", _groups_json(ADOPT_GROUP)]
    )
    assert result.exit_code == 0, result.output
    assert g.read_text() == settled

    # Adoption itself is a no-op from run 2 on: nothing minted, nothing
    # deleted, no parent moved, and the receipt says so.
    assert {e["id"] for e in read_entries()} == ids_after_first
    assert {e["id"]: e.get("parent") for e in read_entries()} == parents_after_first
    assert json.loads(result.stdout)["groups"][0]["adopted"] == []


def test_adopt_same_id_in_two_groups_is_refused(graph_env):
    """AC3: ambiguous claim fails in validate_groups, before any graph write."""
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"))
    before = read_entries()
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "zeta", "title": "Zeta", "waves": "1", "adopt": ["ab-kid00001"]},
            {"slug": "kappa", "title": "Kappa", "waves": "2", "adopt": ["ab-kid00001"]},
        ])]
    )
    assert result.exit_code == 1, result.output
    # Non-English slugs: "one" would also match the refusal's own trailing
    # "it can belong to one", so the assertion would pass unnamed.
    assert "zeta" in result.output and "kappa" in result.output
    assert read_entries() == before


def test_adopt_unresolvable_id_is_refused(graph_env):
    """AC4: exit 3 (not found) naming the id, graph unmodified."""
    g, read_entries = graph_env
    before = read_entries()
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-nosuch01"]},
        ])]
    )
    assert result.exit_code == 3, result.output
    assert "ab-nosuch01" in result.output
    assert read_entries() == before


def test_adopt_existing_group_child_is_refused(graph_env):
    """AC5: demoting a group into a task would silently reshape the epic."""
    g, read_entries = graph_env
    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)]
    ).exit_code == 0
    victim = _child(read_entries(), "1")["id"]
    before = read_entries()

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(
            THREE_GROUPS + [{"slug": "4", "title": "Group 4", "waves": "7",
                             "adopt": [victim]}]
        )]
    )
    assert result.exit_code == 2, result.output
    assert victim in result.output
    assert read_entries() == before


def test_adopt_self_is_refused(graph_env):
    """A group naming its own resolved id resolves a group_slug by construction."""
    g, read_entries = graph_env
    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)]
    ).exit_code == 0
    own = _child(read_entries(), "1")["id"]
    before = read_entries()

    spec = [dict(g0) for g0 in THREE_GROUPS]
    spec[0]["adopt"] = [own]
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(spec)]
    )
    assert result.exit_code == 2, result.output
    # Self-adoption also trips the cycle guard, which exits 2 as well - assert
    # the message so deleting the group-child guard cannot leave this green.
    assert "already the group child for slug" in result.output
    assert read_entries() == before


def test_adopt_the_epic_itself_is_refused(graph_env):
    g, read_entries = graph_env
    before = read_entries()
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-epic0001"]},
        ])]
    )
    assert result.exit_code == 1, result.output
    assert read_entries() == before


def test_adopt_ancestor_of_the_epic_is_refused_as_a_cycle(graph_env):
    """The re-parent cycle guard cli.py already carried for exactly this path."""
    g, read_entries = graph_env
    entries = json.loads(g.read_text())["entries"]
    entries.append(_node("ab-grand0001"))
    for e in entries:
        if e["id"] == "ab-epic0001":
            e["parent"] = "ab-grand0001"
    g.write_text(json.dumps({"entries": entries}) + "\n")
    before = read_entries()

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-grand0001"]},
        ])]
    )
    assert result.exit_code == 2, result.output
    assert "would create a cycle" in result.output
    assert read_entries() == before


@pytest.mark.parametrize("bad", ["x-261c", 7, [""], [None], [7]])
def test_adopt_shape_errors_are_refused(graph_env, bad):
    g, read_entries = graph_env
    before = read_entries()
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": bad},
        ])]
    )
    assert result.exit_code == 1, result.output
    assert "adopt" in result.output
    assert read_entries() == before


def test_adopt_a_shipped_node_is_permitted(graph_env):
    """Re-parenting changes rollup membership, not delivery state.

    The `is_shipped` refusal stays scoped to ORPHANED group children, which is
    a different operation - refusing here would strand exactly the
    evidence-carrying nodes adoption exists to preserve.
    """
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001", pr_number=612,
                                  merge_status="merged", status="done"))
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]},
        ])]
    )
    assert result.exit_code == 0, result.output
    entries = read_entries()
    kid = next(e for e in entries if e["id"] == "ab-kid00001")
    assert kid["parent"] == _child(entries, "one")["id"]
    assert kid["pr_number"] == 612


# -- contained_in: the delivery-unit record adoption writes (x-e957 task 1.2) --


def test_adopt_records_contained_in_naming_the_delivery_unit(graph_env):
    """AC3: adoption stamps the owning group child, not just the parent pointer.

    `parent` says "belongs to"; `contained_in` says "ships inside that node's
    PR". They coincide today, and the second is still the load-bearing one:
    `parent` is a tree edge every epic child carries, so a reader keying on it
    could not tell a normal epic child from a node with no PR of its own.
    """
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"), _epic_child("ab-kid00002"),
                   _epic_child("ab-kid00003"))
    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(ADOPT_GROUP)]
    ).exit_code == 0

    entries = read_entries()
    unit = _child(entries, "appendix")["id"]
    by_id = {e["id"]: e for e in entries}
    for kid in ("ab-kid00001", "ab-kid00002", "ab-kid00003"):
        assert by_id[kid]["contained_in"] == unit
    # The delivery unit is NOT contained in itself - it is the thing that ships.
    assert by_id[unit].get("contained_in") is None


def test_contained_in_survives_a_later_unrelated_mutation(graph_env):
    """AC3: the record is durable, which is the whole reason it is a field.

    Status is recomputed on every write, so an adopted node carrying a plan and
    no blockers derives `ready` again on the next mutation - armed exactly as
    before. A completion_note would be equally durable but not queryable; what
    matters here is that recompute + canonicalize round-trip the field rather
    than dropping it as an unknown extra.
    """
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001", plan_path="/p/one.md"))
    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]},
        ])]
    ).exit_code == 0
    unit = _child(read_entries(), "one")["id"]

    # Any other locked mutation: a priority bump on an unrelated node.
    assert _invoke(
        ["backlog", "update", "ab-epic0001", "--priority", "p0", "--blocks-everything"]
    ).exit_code == 0

    kid = next(e for e in read_entries() if e["id"] == "ab-kid00001")
    assert kid["contained_in"] == unit


def test_adopt_backfills_contained_in_on_an_already_reparented_node(graph_env):
    """A node adopted before this field existed converges on the next run.

    The stamp goes BEFORE the already-adopted short-circuit for exactly this:
    keying it off the re-parent would leave every pre-existing adopted node
    permanently half-adopted, parented but still armed, with no verb to fix it.
    """
    g, read_entries = graph_env
    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1"},
        ])]
    ).exit_code == 0
    unit = _child(read_entries(), "one")["id"]

    # Hand-build the legacy shape: parented to the group child, no contained_in.
    _seed_children(g, _node("ab-kid00001", parent=unit, status="ready"))
    assert next(
        e for e in read_entries() if e["id"] == "ab-kid00001"
    ).get("contained_in") is None

    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]},
        ])]
    ).exit_code == 0
    assert next(
        e for e in read_entries() if e["id"] == "ab-kid00001"
    )["contained_in"] == unit


def test_ac10_no_adopt_key_leaves_contained_in_off_the_wire_entirely(graph_env):
    """AC10: a containment-free graph serializes exactly as it did pre-change.

    The field is listed in CANONICAL_FIELD_ORDER but deliberately never
    setdefault-ed. Defaulting it beside the other nullable scalars would stamp
    `"contained_in": null` onto every node in every graph in existence - which
    reads as harmless and is precisely the byte-level change AC10 forbids.
    Asserting on the raw bytes, not on read_entries(), because the read path is
    what would paper over a setdefault.
    """
    g, _read_entries = graph_env
    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)]
    ).exit_code == 0
    assert "contained_in" not in g.read_text()


def test_adopt_rerun_leaves_contained_in_byte_stable(graph_env):
    """AC10 sibling: re-stamping the same value keeps the graph settled.

    Convergence and idempotency have to hold together - writing the field
    unconditionally is only safe because writing the value it already holds
    changes no bytes.
    """
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"), _epic_child("ab-kid00002"),
                   _epic_child("ab-kid00003"))
    spec = ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(ADOPT_GROUP)]
    assert _invoke(spec).exit_code == 0
    assert _invoke(spec).exit_code == 0
    settled = g.read_text()
    assert _invoke(spec).exit_code == 0
    assert g.read_text() == settled
    assert '"contained_in"' in settled


# -- warn on epic children that no group adopted (x-b9d7 US3) --


def test_unadopted_epic_child_warns_and_still_exits_zero(graph_env):
    """AC6: a half-packaged epic is visible without the run failing."""
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001", title="left behind"))
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)]
    )
    assert result.exit_code == 0, result.output
    assert "ab-kid00001" in result.output
    assert "adopt" in result.output


def test_adopted_children_are_not_warned_about(graph_env):
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"), _epic_child("ab-kid00002"),
                   _epic_child("ab-kid00003"))
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(ADOPT_GROUP)]
    )
    assert result.exit_code == 0, result.output
    assert "adopted by no group" not in result.output


def test_no_adopt_key_against_a_clean_epic_is_unchanged(graph_env):
    """AC7 regression guard: the overwhelmingly common path must not move."""
    g, read_entries = graph_env
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)]
    )
    assert result.exit_code == 0, result.output
    assert "adopted by no group" not in result.output
    children = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    assert len(children) == 3


def test_unadopted_warning_reaches_the_json_path_too(graph_env):
    """A --json caller decomposing a populated epic needs the warning as much.

    Guarding only the human report would leave the JSON path silently
    duplicating an epic - and stderr never pollutes the payload on stdout.
    """
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001", title="left behind"))
    result = _invoke(
        ["--json", "backlog", "decompose", "ab-epic0001",
         "--groups", _groups_json(THREE_GROUPS)]
    )
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["epic"] == "ab-epic0001"
    assert "ab-kid00001" in result.stderr


# -- adopt: review findings (PR #655) --


def test_adopt_aliased_ids_in_two_groups_are_refused(graph_env):
    """Two spellings of one id must not slip past the dual-claim refusal.

    `_find_node` resolves a 4-7 hex `ab-` prefix to the same entry as the full
    id, so a string-equality check in validate_groups sees two distinct claims.
    Without a resolved-id check the first group re-parents the node and the
    second re-parents it again, exiting 0 with the node silently in group two.
    """
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-abcd0001", title="claimed twice"))
    before = read_entries()

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-abcd"]},
            {"slug": "two", "title": "Two", "waves": "2", "adopt": ["ab-abcd0001"]},
        ])]
    )
    assert result.exit_code == 1, result.output
    assert "one" in result.output and "two" in result.output
    assert read_entries() == before


def test_adopt_aliased_ids_within_one_group_are_refused(graph_env):
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-abcd0001"))
    before = read_entries()

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1",
             "adopt": ["ab-abcd", "ab-abcd0001"]},
        ])]
    )
    assert result.exit_code == 1, result.output
    assert read_entries() == before


@pytest.mark.parametrize("falsy", [False, 0, "", {}, None])
def test_adopt_present_but_falsy_non_list_is_refused(graph_env, falsy):
    """A present-but-invalid `adopt` must fail closed, never read as absent.

    `.get("adopt") or []` collapses every falsy value to the mint-only default,
    so a malformed spec would proceed and leave existing epic children
    unadopted while minting new group nodes - the exact outcome adoption
    exists to prevent. Mirrors the module's `max_children` rule, where an
    explicit null fails closed instead of masquerading as absent.
    """
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-abcd0001"))
    before = read_entries()

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": falsy},
        ])]
    )
    assert result.exit_code == 1, result.output
    assert "adopt" in result.output
    assert read_entries() == before


def test_adopted_ids_reach_the_human_receipt(graph_env):
    """Re-parenting pre-existing nodes must be visible without `--json`.

    Same rule the fan-out ownership line follows: a contract carried only in
    the JSON shape is a contract the default invocation never shows.
    """
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"), _epic_child("ab-kid00002"),
                   _epic_child("ab-kid00003"))
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(ADOPT_GROUP)]
    )
    assert result.exit_code == 0, result.output
    assert "adopted=" in result.stdout
    for kid in ("ab-kid00001", "ab-kid00002", "ab-kid00003"):
        assert kid in result.stdout


def test_receipt_omits_adopted_when_nothing_was_adopted(graph_env):
    """AC7: a spec with no adopt key prints exactly what it printed before."""
    g, read_entries = graph_env
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)]
    )
    assert result.exit_code == 0, result.output
    assert "adopted=" not in result.stdout


def test_adopt_cannot_steal_a_legacy_group_child_of_another_epic(graph_env, tmp_path):
    """A group child born before `group_slug` is invisible to a base-scoped check.

    `group_child_slug` answers "group child of THIS doc", so a legacy child of a
    DIFFERENT epic resolves None here. Adopting it would re-parent it away from
    its own epic, whose next decompose no longer finds it (the upsert lookup is
    scoped to parent == epic) and mints a duplicate for that slug.
    """
    g, read_entries = graph_env
    doc_b = tmp_path / "epicB.md"
    doc_b.write_text("---\ntitle: B\n---\n")
    _seed_children(
        g,
        _node("ab-epicB001", title="Epic B", plan_path=str(doc_b)),
        _node("ab-b0010001", parent="ab-epicB001", title="B's api group",
              plan_path=str(tmp_path / "epicB.group-api.md")),
    )
    before = read_entries()

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-b0010001"]},
        ])]
    )
    assert result.exit_code == 2, result.output
    assert "another epic" in result.output
    assert read_entries() == before


def test_adopt_cannot_steal_a_modern_group_child_of_another_epic(graph_env, tmp_path):
    g, read_entries = graph_env
    _seed_children(
        g,
        _node("ab-epicB001", title="Epic B", plan_path=str(tmp_path / "epicB.md")),
        _node("ab-b0010001", parent="ab-epicB001", title="B's api group",
              group_slug="api"),
    )
    before = read_entries()

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-b0010001"]},
        ])]
    )
    assert result.exit_code == 2, result.output
    # NOTE: the base-scoped check already refused this one, because group_slug
    # short-circuits independently of base. Only the LEGACY sibling above
    # exercises the unscoped predicate; this pins the message either way.
    assert "already the group child for slug 'api'" in result.output
    assert read_entries() == before


def test_adopt_same_id_twice_in_one_group_says_so(graph_env):
    """The cross-group wording reads as self-contradicting for a copy-paste typo."""
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"))
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1",
             "adopt": ["ab-kid00001", "ab-kid00001"]},
        ])]
    )
    assert result.exit_code == 1, result.output
    assert "twice" in result.output
    assert "both group" not in result.output


def test_adopt_lands_each_node_under_ITS_group_not_merely_a_group(graph_env):
    """With one group in the spec, "right group" and "only group" look identical.

    Every other destination assertion here uses a single-group spec, so a defect
    that hoisted the resolved node out of the per-group loop, or keyed the
    re-parent off the wrong slug, would pass the whole suite.
    """
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"), _epic_child("ab-kid00002"),
                   _epic_child("ab-kid00003"))
    result = _invoke(
        ["--json", "backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "alpha", "title": "Alpha", "waves": "1",
             "adopt": ["ab-kid00001"]},
            {"slug": "beta", "title": "Beta", "waves": "2",
             "adopt": ["ab-kid00002", "ab-kid00003"]},
        ])]
    )
    assert result.exit_code == 0, result.output

    entries = read_entries()
    alpha, beta = _child(entries, "alpha")["id"], _child(entries, "beta")["id"]
    assert alpha != beta
    by_id = {e["id"]: e for e in entries}
    assert by_id["ab-kid00001"]["parent"] == alpha
    assert by_id["ab-kid00002"]["parent"] == beta
    assert by_id["ab-kid00003"]["parent"] == beta

    payload = {g0["slug"]: g0["adopted"] for g0 in json.loads(result.stdout)["groups"]}
    assert payload == {"alpha": ["ab-kid00001"],
                       "beta": ["ab-kid00002", "ab-kid00003"]}


def test_adopt_rehoming_a_node_to_another_group_moves_it(graph_env):
    """Re-running with the node moved to a different group must relocate it."""
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"))
    spec = [
        {"slug": "alpha", "title": "Alpha", "waves": "1", "adopt": ["ab-kid00001"]},
        {"slug": "beta", "title": "Beta", "waves": "2"},
    ]
    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(spec)]
    ).exit_code == 0
    alpha = _child(read_entries(), "alpha")["id"]
    assert next(e for e in read_entries() if e["id"] == "ab-kid00001")["parent"] == alpha

    spec[0].pop("adopt")
    spec[1]["adopt"] = ["ab-kid00001"]
    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(spec)]
    ).exit_code == 0
    beta = _child(read_entries(), "beta")["id"]
    assert next(e for e in read_entries() if e["id"] == "ab-kid00001")["parent"] == beta


def test_mixed_run_adopts_some_and_warns_about_the_rest(graph_env):
    """The motivating real case: one run that both re-parents and warns.

    Also the only exercise of the plural warning path (count > 1, comma join);
    the other warning tests seed exactly one child.
    """
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"), _epic_child("ab-kid00002"),
                   _epic_child("ab-kid00003"))
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "alpha", "title": "Alpha", "waves": "1",
             "adopt": ["ab-kid00001"]},
        ])]
    )
    assert result.exit_code == 0, result.output
    alpha = _child(read_entries(), "alpha")["id"]
    by_id = {e["id"]: e for e in read_entries()}
    assert by_id["ab-kid00001"]["parent"] == alpha
    assert by_id["ab-kid00002"]["parent"] == "ab-epic0001"
    assert by_id["ab-kid00003"]["parent"] == "ab-epic0001"

    assert "2 epic child(ren) adopted by no group" in result.stderr
    assert "ab-kid00002, ab-kid00003" in result.stderr
    assert "ab-kid00001" not in result.stderr  # adopted, so it excluded itself


def test_adopting_a_plain_child_of_another_epic_is_allowed(graph_env, tmp_path):
    """Pins the chosen line: adoption moves plain nodes, never group boundaries.

    The design says an adopted node is re-parented "from wherever it sits", so a
    plain child of another epic is legal. A GROUP child of another epic is not
    (see the steal tests) - that would strand its epic and mint a duplicate.
    """
    g, read_entries = graph_env
    _seed_children(
        g,
        _node("ab-epicB001", title="Epic B", plan_path=str(tmp_path / "epicB.md")),
        _node("ab-b0020001", parent="ab-epicB001", title="B's plain child"),
    )
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-b0020001"]},
        ])]
    )
    assert result.exit_code == 0, result.output
    entries = read_entries()
    assert next(e for e in entries if e["id"] == "ab-b0020001")["parent"] == (
        _child(entries, "one")["id"]
    )


def test_warning_never_recommends_a_node_the_refusal_blocks(graph_env, tmp_path):
    """The warning and the adoption refusal must share one predicate.

    This epic's own group child on a plan path that no longer matches a renamed
    epic doc resolves no base-scoped slug. Keying the warning on that resolved
    slug listed it as un-adopted, so the warning told the operator to adopt a
    node the refusal then blocked - with no --force escape.
    """
    g, read_entries = graph_env
    _seed_children(
        g,
        _node("ab-old00001", parent="ab-epic0001", title="own legacy group",
              plan_path=str(tmp_path / "old.group-api.md")),
    )
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "api", "title": "Api", "waves": "1"},
        ])]
    )
    assert result.exit_code == 0, result.output
    assert "ab-old00001" not in result.stderr


def test_own_legacy_group_child_refusal_does_not_blame_another_epic(graph_env, tmp_path):
    g, read_entries = graph_env
    _seed_children(
        g,
        _node("ab-old00001", parent="ab-epic0001", title="own legacy group",
              plan_path=str(tmp_path / "old.group-api.md")),
    )
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "api", "title": "Api", "waves": "1", "adopt": ["ab-old00001"]},
        ])]
    )
    assert result.exit_code == 2, result.output
    assert "another epic" not in result.output
    assert "this epic's group child on a legacy plan path" in result.output


def test_adopt_names_the_epic_by_prefix_exits_1_not_as_a_cycle(graph_env, tmp_path):
    """The epic-self refusal must survive an aliasable spelling.

    `validate_groups` compares the raw epic argument, so a 4-7 hex `ab-` prefix
    of the epic reaches Pass 1 and used to land on the generic cycle refusal
    (exit 2) rather than the purpose-built one (exit 1).
    """
    g, read_entries = graph_env
    doc = tmp_path / "e2.md"
    doc.write_text("---\ntitle: E2\n---\n")
    _seed_children(g, _node("ab-abcd0001", title="Epic 2", plan_path=str(doc),
                            status="ready"))
    before = read_entries()

    result = _invoke(
        ["backlog", "decompose", "ab-abcd0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-abcd"]},
        ])]
    )
    assert result.exit_code == 1, result.output
    assert "names the epic" in result.output
    assert read_entries() == before


@pytest.mark.parametrize("spec,code", [
    ([{"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-nosuch01"]}], 3),
    ([{"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-epic0001"]}], 1),
    ([{"slug": "one", "title": "One", "waves": "1", "adopt": False}], 1),
    ([{"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]},
      {"slug": "two", "title": "Two", "waves": "2", "adopt": ["ab-kid00001"]}], 1),
])
def test_every_refusal_leaves_the_graph_byte_identical(graph_env, spec, code):
    """The atomicity contract is byte-level, not parsed-equality.

    The other refusal tests compare `read_entries()`, which would stay green if
    a refusal path rewrote the file with different key order or whitespace.
    """
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"))
    raw_before = g.read_bytes()

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(spec)]
    )
    assert result.exit_code == code, result.output
    assert g.read_bytes() == raw_before


def test_adopt_refuses_a_node_a_live_worker_is_building(graph_env, monkeypatch):
    """codex P1: adoption racing a dispatch left a worker on a contained node.

    Every dispatch gate reads containment BEFORE the claim, so adoption landing
    in that window produced a session holding a claim on a node that no longer
    dispatches - and it built and opened its own PR regardless. Closed from the
    adoption side because that is the single boundary: it covers every dispatch
    path at once and prevents the contradictory state instead of killing a
    worker mid-flight.
    """
    import fno.graph.cli as gcli

    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"))
    before = g.read_text()
    monkeypatch.setattr(gcli, "_live_worker",
                        lambda nid: "target-session:S1" if nid == "ab-kid00001" else None)

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]},
        ])]
    )
    assert result.exit_code == 2, result.output
    assert "being built right now" in result.output
    assert "target-session:S1" in result.output
    # Atomic: a refused decompose leaves the graph untouched.
    assert g.read_text() == before


def test_adopt_proceeds_when_no_worker_holds_the_node(graph_env, monkeypatch):
    """The guard must key on a LIVE claim, not on merely having been claimed."""
    import fno.graph.cli as gcli

    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"))
    monkeypatch.setattr(gcli, "_live_worker", lambda nid: None)

    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]},
        ])]
    ).exit_code == 0
    entries = read_entries()
    assert next(e for e in entries if e["id"] == "ab-kid00001")["contained_in"] \
        == _child(entries, "one")["id"]


def test_remove_clears_containment_on_its_children(graph_env, monkeypatch):
    """codex P2: a child left naming a deleted unit is a permanent trap.

    Selection and `fno do target init` both refuse it, while the reconcile heal
    deliberately skips a missing owner - so it is unbuildable, uncloseable, and
    invisible to every sweep. Un-contained rather than closed: deleting the unit
    is not a claim that its children shipped.
    """
    import fno.graph.cli as gcli

    g, read_entries = graph_env
    monkeypatch.setattr(gcli, "_live_worker", lambda nid: None)
    _seed_children(g, _epic_child("ab-kid00001"))
    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]},
        ])]
    ).exit_code == 0
    unit = _child(read_entries(), "one")["id"]

    assert _invoke(["backlog", "remove", unit, "--force"]).exit_code == 0
    kid = next(e for e in read_entries() if e["id"] == "ab-kid00001")
    assert kid.get("contained_in") is None
    assert kid.get("completed_at") is None


def test_adopt_does_not_contain_a_node_that_owns_a_pr(graph_env, monkeypatch):
    """codex P1: `contained_in` says "this node has no PR of its own".

    A PR-bearing adoptee has one. Stamping it anyway would hide an open PR's
    node from dispatch, auto-close it under someone else's merge while its own
    PR is still open, and report a finished one as having "shipped inside" a
    unit it predates. Adoption still proceeds - re-parenting changes rollup
    membership, not delivery state, which test_adopt_a_shipped_node_is_permitted
    pins - only the stamp is withheld, and loudly.
    """
    import fno.graph.cli as gcli

    g, read_entries = graph_env
    monkeypatch.setattr(gcli, "_live_worker", lambda nid: None)
    _seed_children(g, _epic_child("ab-kid00001", pr_number=612,
                                  merge_status="merged", status="done"))
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]},
        ])]
    )
    assert result.exit_code == 0, result.output
    entries = read_entries()
    kid = next(e for e in entries if e["id"] == "ab-kid00001")
    # Re-parented (adoption happened) but NOT contained.
    assert kid["parent"] == _child(entries, "one")["id"]
    assert kid.get("contained_in") is None
    assert kid["pr_number"] == 612
    assert "did NOT mark it contained" in result.output
    assert "612" in result.output


def test_adopt_does_not_contain_a_node_that_already_carries_cost(graph_env,
                                                                 monkeypatch):
    """Same rule for cost, and for a reason the rollup guard cannot reach.

    `_apply_rollup` reads an empty rollup as "preserve existing", so a node that
    already carries `cost_usd` keeps it and stays in the flat project sum - the
    double-count the rollup guard prevents for NEW attribution would simply
    persist for old.
    """
    import fno.graph.cli as gcli

    g, read_entries = graph_env
    monkeypatch.setattr(gcli, "_live_worker", lambda nid: None)
    # Landed: an UNFINISHED cost-carrying node is refused outright (it is a
    # delivery unit mid-flight); this covers the completed case, where the
    # measurement is history and only the stamp is withheld.
    _seed_children(g, _epic_child("ab-kid00001", cost_usd=4.25, status="done",
                                  completed_at="2026-07-01T00:00:00+00:00"))
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]},
        ])]
    )
    assert result.exit_code == 0, result.output
    kid = next(e for e in read_entries() if e["id"] == "ab-kid00001")
    assert kid.get("contained_in") is None
    assert kid["cost_usd"] == 4.25
    assert "did NOT mark it contained" in result.output


def test_adopt_refuses_a_node_with_descendants(graph_env, monkeypatch):
    """codex P1: containment is one level, so a subtree would half-close.

    `selection_guards` does not treat a contained ANCESTOR as a guard, so the
    children stay independently dispatchable and open their own PRs while the
    merge cascade closes only the parent. Refusing beats propagating: a subtree
    is a decomposition of its own, and folding it wholesale into another unit is
    a reshape the operator should state.
    """
    import fno.graph.cli as gcli

    g, read_entries = graph_env
    monkeypatch.setattr(gcli, "_live_worker", lambda nid: None)
    _seed_children(g, _epic_child("ab-kid00001"),
                   _node("ab-sub00001", parent="ab-kid00001", status="ready"))
    before = g.read_text()
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]},
        ])]
    )
    assert result.exit_code == 2, result.output
    assert "one level" in result.output
    assert "ab-sub00001" in result.output
    assert g.read_text() == before


def test_rehoming_between_groups_still_restamps_containment(graph_env, monkeypatch):
    """Rehoming is a SUPPORTED operation here, so containment follows the move.

    An external reviewer proposed refusing an adoptee that already carries a
    different `contained_in`. That would break
    test_adopt_rehoming_a_node_to_another_group_moves_it: after a rehome the old
    owner never delivers the node, so re-stamping is the correct write, not a
    silent ownership theft.
    """
    import fno.graph.cli as gcli

    g, read_entries = graph_env
    monkeypatch.setattr(gcli, "_live_worker", lambda nid: None)
    _seed_children(g, _epic_child("ab-kid00001"))
    spec = [
        {"slug": "alpha", "title": "Alpha", "waves": "1", "adopt": ["ab-kid00001"]},
        {"slug": "beta", "title": "Beta", "waves": "2"},
    ]
    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(spec)]
    ).exit_code == 0
    alpha = _child(read_entries(), "alpha")["id"]
    assert next(e for e in read_entries()
                if e["id"] == "ab-kid00001")["contained_in"] == alpha

    spec[0].pop("adopt")
    spec[1]["adopt"] = ["ab-kid00001"]
    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(spec)]
    ).exit_code == 0
    beta = _child(read_entries(), "beta")["id"]
    assert next(e for e in read_entries()
                if e["id"] == "ab-kid00001")["contained_in"] == beta


def test_backfill_still_refuses_an_adoptee_that_gained_children(graph_env,
                                                                monkeypatch):
    """sigma: the descendants guard was unreachable on the BACK-FILL path.

    The `already adopted -> continue` short-circuit preceded it, so re-running a
    spec against a legacy adopted node that had since gained children stamped it
    anyway - producing the exact half-closed subtree the refusal exists to
    prevent. Both guards now run before the stamp.
    """
    import fno.graph.cli as gcli

    g, read_entries = graph_env
    monkeypatch.setattr(gcli, "_live_worker", lambda nid: None)
    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1"},
        ])]
    ).exit_code == 0
    unit = _child(read_entries(), "one")["id"]

    # The legacy shape: parented to the group child, no contained_in, and it has
    # since gained a child of its own.
    _seed_children(g, _node("ab-kid00001", parent=unit, status="ready"),
                   _node("ab-sub00001", parent="ab-kid00001", status="ready"))

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]},
        ])]
    )
    assert result.exit_code == 2, result.output
    assert "one level" in result.output
    assert next(
        e for e in read_entries() if e["id"] == "ab-kid00001"
    ).get("contained_in") is None


def test_supersede_releases_its_contained_children(graph_env, monkeypatch):
    """sigma: the same trap cmd_remove was fixed for, one step short of deletion.

    A superseded unit will never merge, so `_strandable_contained_ids` (keyed on
    completed_at) never heals its children while selection keeps refusing them
    and the redirect keeps pointing at a node that is not going to ship.
    """
    import fno.graph.cli as gcli

    g, read_entries = graph_env
    monkeypatch.setattr(gcli, "_live_worker", lambda nid: None)
    _seed_children(g, _epic_child("ab-kid00001"))
    assert _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]},
        ])]
    ).exit_code == 0
    entries = read_entries()
    unit = _child(entries, "one")["id"]
    assert next(e for e in entries if e["id"] == "ab-kid00001")["contained_in"] == unit

    _seed_children(g, _node("ab-new00001", status="ready"))
    assert _invoke(
        ["backlog", "supersede", "ab-new00001", "--replaces", unit,
         "--cause", "regrouped", "--surface", "x.py"]
    ).exit_code == 0

    kid = next(e for e in read_entries() if e["id"] == "ab-kid00001")
    assert kid.get("contained_in") is None, "child left pointing at a dead unit"
    # Released, not closed: superseding the unit is not a claim its work shipped.
    assert kid.get("completed_at") is None


def test_adopt_refuses_an_unfinished_node_that_owns_a_pr(graph_env, monkeypatch):
    """codex P1: withholding the stamp is not enough for a node still in flight.

    It stays dispatchable, but re-parenting still hangs it under the group child
    - and `_cascade_close_parents` only asks whether the EPIC's direct children
    are complete, never its grandchildren. So the group's merge would close the
    epic, and dispatch its dependents, over open work one level down.
    """
    import fno.graph.cli as gcli

    g, read_entries = graph_env
    monkeypatch.setattr(gcli, "_live_worker", lambda nid: None)
    _seed_children(g, _epic_child("ab-kid00001", pr_number=613, status="ready"))
    before = g.read_text()

    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]},
        ])]
    )
    assert result.exit_code == 2, result.output
    assert "613" in result.output
    assert "has not landed" in result.output
    assert g.read_text() == before


def test_adopt_still_permits_a_landed_pr_bearing_node(graph_env, monkeypatch):
    """The completed case stays permitted - it is history, not open work.

    `test_adopt_a_shipped_node_is_permitted` pins that adoption changes rollup
    membership rather than delivery state, and a done node cannot strand an
    epic-level cascade because it is already complete.
    """
    import fno.graph.cli as gcli

    g, read_entries = graph_env
    monkeypatch.setattr(gcli, "_live_worker", lambda nid: None)
    _seed_children(g, _epic_child("ab-kid00001", pr_number=612,
                                  merge_status="merged", status="done",
                                  completed_at="2026-07-01T00:00:00+00:00"))
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json([
            {"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]},
        ])]
    )
    assert result.exit_code == 0, result.output
    kid = next(e for e in read_entries() if e["id"] == "ab-kid00001")
    assert kid.get("contained_in") is None
    assert kid["pr_number"] == 612


# -- a dead delivery unit cannot own containment --


def _decompose(groups):
    return _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(groups)]
    )


ADOPT_ONE = [{"slug": "one", "title": "One", "waves": "1", "adopt": ["ab-kid00001"]}]
BARE_ONE = [{"slug": "one", "title": "One", "waves": "1"}]


def _deferred_unit(g, read_entries):
    """Seed one adoptable child, mint group child `one`, then defer it."""
    _seed_children(g, _epic_child("ab-kid00001"))
    assert _decompose(BARE_ONE).exit_code == 0
    unit = _child(read_entries(), "one")["id"]
    assert _invoke(["backlog", "defer", unit, "--reason", "parked"]).exit_code == 0
    return unit


def _legacy_deferred_unit(g, read_entries):
    """Seed one adoptable child, mint group child `one`, then park it with the
    pre-feature workaround: ``completed_at: "deferred:<ts>"`` and no deferred_at.

    ``recompute_statuses`` migrates this marker to ``deferred_at``, but only
    AFTER a mutator returns, so the decompose mutator sees it raw - exactly the
    shape that bypassed the guard before the legacy-prefix fix.
    """
    _seed_children(g, _epic_child("ab-kid00001"))
    assert _decompose(BARE_ONE).exit_code == 0
    unit = _child(read_entries(), "one")["id"]
    entries = json.loads(g.read_text())["entries"]
    parked = next(e for e in entries if e["id"] == unit)
    parked["completed_at"] = "deferred:2026-07-01T00:00:00+00:00"
    parked.pop("deferred_at", None)
    g.write_text(json.dumps({"entries": entries}) + "\n")
    return unit


def test_adopt_under_a_deferred_group_child_is_refused(graph_env):
    """AC1: the stamp would be unreachable by every repair path that exists.

    `_release_contained_children` already ran when the unit was deferred and
    nothing re-runs it; `_strandable_contained_ids` keys on the owner's
    completed_at, which a deferred owner has not got. So a re-stamped adoptee is
    refused by both dispatch halves forever, with `update --parent null` as the
    only escape. Refusing at the single write site is what keeps that state from
    existing at all.
    """
    g, read_entries = graph_env
    unit = _deferred_unit(g, read_entries)

    result = _decompose(ADOPT_ONE)
    assert result.exit_code == 2, result.output
    assert unit in result.output
    assert "deferred" in result.output
    assert "undefer" in result.output
    kid = next(e for e in read_entries() if e["id"] == "ab-kid00001")
    assert kid.get("contained_in") is None


def test_adopt_under_a_legacy_deferred_group_child_is_refused(graph_env):
    """The pre-feature ``completed_at: "deferred:<ts>"`` workaround must not
    bypass the guard.

    ``recompute_statuses`` migrates the marker to ``deferred_at`` only after the
    mutator returns, so reading ``completed_at`` raw would treat a deferred owner
    as done, skip the guard, and stamp the permanently-contained state it exists
    to prevent. The marker is folded into the effective deferred_at for the
    deadness check, so the guard fires identically to the deferred_at case.
    """
    g, read_entries = graph_env
    unit = _legacy_deferred_unit(g, read_entries)

    result = _decompose(ADOPT_ONE)
    assert result.exit_code == 2, result.output
    assert unit in result.output
    assert "deferred" in result.output
    kid = next(e for e in read_entries() if e["id"] == "ab-kid00001")
    assert kid.get("contained_in") is None


def test_adopt_after_undeferring_the_group_child_succeeds(graph_env):
    """AC2: the refusal is a gate, not a wall - the named remedy actually works.

    `cmd_undefer` clears deferred_at and deliberately does NOT re-contain, so
    the stamp arrives through decompose's own back-fill convergence leg rather
    than through the undefer.
    """
    g, read_entries = graph_env
    unit = _deferred_unit(g, read_entries)
    assert _decompose(ADOPT_ONE).exit_code == 2
    assert _invoke(["backlog", "undefer", unit]).exit_code == 0

    result = _decompose(ADOPT_ONE)
    assert result.exit_code == 0, result.output
    kid = next(e for e in read_entries() if e["id"] == "ab-kid00001")
    assert kid["contained_in"] == unit


def test_a_deferred_group_child_with_no_adopt_list_still_updates(graph_env):
    """AC3: the guard is gated on the adopt list, not on the group.

    Refusing every re-run against a deferred group child would deadlock an epic
    doc whose group was parked for unrelated reasons - including by
    `maintain --apply`, which defers without the operator choosing it. With no
    adoptees there is no stamp to withhold, so nothing is at stake.
    """
    g, read_entries = graph_env
    unit = _deferred_unit(g, read_entries)

    result = _decompose([
        {"slug": "one", "title": "One renamed", "waves": "2",
         "blocked_by_groups": ["two"]},
        {"slug": "two", "title": "Two", "waves": "1"},
    ])
    assert result.exit_code == 0, result.output
    entries = read_entries()
    child = next(e for e in entries if e["id"] == unit)
    assert child["title"] == "One renamed"
    assert child["blocked_by"] == [_child(entries, "two")["id"]]
    assert child["deferred_at"]


def test_adopt_under_a_superseded_group_child_names_the_supersession(graph_env):
    """AC4: the remedy has to branch, because `undefer` is not total.

    `cmd_supersede` sets superseded_by AND deferred_at, but `cmd_undefer`
    clears only the second. Offering undefer here would send the operator
    through a command that exits 0, changes nothing this guard reads, and
    refuses identically on the next run.
    """
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"), _epic_child("ab-new00001"))
    assert _decompose(BARE_ONE).exit_code == 0
    unit = _child(read_entries(), "one")["id"]
    assert _invoke([
        "backlog", "supersede", "ab-new00001", "--replaces", unit,
        "--cause", "reshaped", "--surface", "x.py",
    ]).exit_code == 0

    result = _decompose(ADOPT_ONE)
    assert result.exit_code == 2, result.output
    assert "ab-new00001" in result.output
    assert "superseded" in result.output
    # The remedy offers `unsupersede` (the verb that clears superseded_by), not
    # `undefer` (which clears only deferred_at, so the next run refuses again).
    assert f"unsupersede {unit}" in result.output
    assert f"undefer {unit}" not in result.output
    assert "clears superseded_by" in result.output
    kid = next(e for e in read_entries() if e["id"] == "ab-kid00001")
    assert kid.get("contained_in") is None


def test_a_refused_decompose_leaves_the_graph_byte_identical(graph_env):
    """AC5: read-and-raise inside the mutator, so nothing is written at all.

    `locked_mutate_graph` calls `_create_backup` only after the mutator
    returns, so a raise means no write, no `.bak`, and no re-rendered `.md`.
    """
    g, read_entries = graph_env
    _deferred_unit(g, read_entries)
    before = g.read_text()
    baks_before = sorted(p.name for p in g.parent.glob("graph.json.bak.*"))

    assert _decompose(ADOPT_ONE).exit_code == 2
    assert g.read_text() == before
    assert sorted(p.name for p in g.parent.glob("graph.json.bak.*")) == baks_before


def test_defer_keeps_an_existing_adoptee_folded(graph_env):
    """A reversible pause preserves the already-authored delivery boundary."""
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"))
    assert _decompose(ADOPT_ONE).exit_code == 0
    unit = _child(read_entries(), "one")["id"]
    assert next(
        e for e in read_entries() if e["id"] == "ab-kid00001"
    )["contained_in"] == unit

    assert _invoke(["backlog", "defer", unit, "--reason", "parked"]).exit_code == 0
    kid = next(e for e in read_entries() if e["id"] == "ab-kid00001")
    assert kid.get("contained_in") == unit

    assert _decompose(ADOPT_ONE).exit_code == 2
    kid = next(e for e in read_entries() if e["id"] == "ab-kid00001")
    assert kid.get("contained_in") == unit
    assert kid["parent"] == unit


def test_a_superseded_but_completed_group_child_still_adopts(graph_env):
    """AC7: completed_at first, reproducing done > superseded > deferred.

    A unit that was superseded and later closed is history, not a dead end: the
    merge cascade and the existing stranded sweep both handle a done owner, so
    refusing here would be a false positive on a healable graph.
    """
    g, read_entries = graph_env
    _seed_children(g, _epic_child("ab-kid00001"), _epic_child("ab-new00001"))
    assert _decompose(BARE_ONE).exit_code == 0
    unit = _child(read_entries(), "one")["id"]
    assert _invoke([
        "backlog", "supersede", "ab-new00001", "--replaces", unit,
        "--cause", "reshaped", "--surface", "x.py",
    ]).exit_code == 0

    # Model what `_apply_completion_fields` does on close: stamp completed_at
    # and clear deferred_at, leaving superseded_by behind.
    entries = json.loads(g.read_text())["entries"]
    closed = next(e for e in entries if e["id"] == unit)
    closed["completed_at"] = "2026-07-01T00:00:00+00:00"
    closed.pop("deferred_at", None)
    g.write_text(json.dumps({"entries": entries}) + "\n")

    result = _decompose(ADOPT_ONE)
    assert result.exit_code == 0, result.output
    kid = next(e for e in read_entries() if e["id"] == "ab-kid00001")
    assert kid["contained_in"] == unit


def test_child_progress_set_matches_the_predicate_it_replaced():
    from fno.graph._intake import _epics_with_child_progress

    id_to_entry = {
        "x-1a2b": {"id": "x-1a2b", "type": "epic"},
        "x-9f0c": {"id": "x-9f0c", "type": "epic"},
        "x-3c4d": {"id": "x-3c4d", "type": "epic"},
        "x-5e6f": {"id": "x-5e6f", "type": "epic"},
        "kid-done": {"id": "kid-done", "parent": "x-1a2b", "completed_at": "t"},
        "kid-run": {"id": "kid-run", "parent": "x-9f0c", "status": "in_progress"},
        "kid-sess": {"id": "kid-sess", "parent": "x-3c4d", "session_id": "s"},
        "kid-idle": {"id": "kid-idle", "parent": "x-5e6f", "status": "ready"},
    }
    assert _epics_with_child_progress(id_to_entry) == frozenset(
        {"x-1a2b", "x-9f0c", "x-3c4d"}
    )


def test_sort_key_orders_live_epic_children_before_their_epic():
    """The whole point of the per-node epic lookup the scan was hiding inside."""
    from fno.graph._intake import make_selection_sort_key

    entries = [
        {"id": "x-1a2b", "type": "epic", "priority": "p1", "status": "in_progress",
         "created_at": "2026-01-01"},
        {"id": "x-9f0c", "parent": "x-1a2b", "priority": "p2", "status": "done",
         "completed_at": "t", "created_at": "2026-01-02"},
        {"id": "x-3c4d", "parent": "x-1a2b", "priority": "p2", "status": "ready",
         "created_at": "2026-01-03"},
    ]
    ordered = [e["id"] for e in sorted(entries, key=make_selection_sort_key(entries))]
    assert ordered.index("x-3c4d") < ordered.index("x-1a2b")
