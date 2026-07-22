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
        "cwd": "/tmp/abilities",
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
    # Filename is the `fno plan path` shape with the child's created_at date...
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


def test_scaffold_born_stub_not_ready():
    # US1: the scaffold is born `status: stub`, never `ready` - the whole point.
    text = scaffold_separate_plan(_grp(), "ab-epic0001", "big.md", why_digest="the why")
    assert "status: stub\n" in text
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
    """Patch both spawn lanes; return (fanout_calls, offer_calls). Each fan-out
    call records a kwargs dict and returns a `spawned` result by default."""
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


def test_flagged_group_forces_fanout_unflagged_offers(graph_env, monkeypatch, tmp_path):
    g, read_entries = graph_env
    fanout, offers = _spy_spawn(monkeypatch)
    groups = [
        {"slug": "1", "title": "G1 spike", "waves": "1", "blocked_by_groups": [],
         "needs_think": True},
        {"slug": "2", "title": "G2 rote", "waves": "2", "blocked_by_groups": ["1"]},
    ]
    result = _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(groups)])
    assert result.exit_code == 0, result.output

    # The flagged child took the fan-out lane with the gate + spawn forced on;
    # the unflagged child got the born-with-why offer.
    assert len(fanout) == 1 and len(offers) == 1
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
        # x-d6a6: born at the canonical `fno plan path` name, not the legacy
        # `.group-<slug>.md`. The legacy path is no longer written.
        f = _canonical(c)
        assert f.exists(), f"scaffold not written: {f}"
        assert f.name.endswith(f"-{c['group_slug']}-{c['id']}.md")
        assert not Path(separate_plan_path(str(doc), c["group_slug"])).exists()
        body = f.read_text()
        assert "status: stub" in body       # born stub, never ready
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
    """AC1-HP: a routed child's stub lands at the canonical `fno plan path` name in
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
