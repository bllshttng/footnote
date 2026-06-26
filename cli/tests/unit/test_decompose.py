"""Tests for `fno backlog decompose` - bounded epic decomposition (ab-e9c81ed3, C1).

The verb upserts group child nodes under an epic in a single locked graph
mutation: atomic (all-or-nothing) and idempotent (keyed on parent + the
`#group-<slug>` plan fragment). Covers AC1-HP, AC1-ERR, AC1-UI, AC1-EDGE,
AC1-FR from internal/fno/plans/2026-05-24-epic-scoped-execution.md.
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
        "_status": "idea",
        "created_at": "2026-01-01T00:00:00+00:00",
    }
    base.update(overrides)
    return base


@pytest.fixture
def graph_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Temp graph.json wired into the CLI; returns (path, read_entries)."""
    import fno.graph._constants as gc
    import fno.graph.store as gs

    g = tmp_path / "graph.json"
    epic = _node(
        "ab-epic0001",
        title="Epic: big thing",
        plan_path="internal/fno/plans/big.md#c1-anchor",
        priority="p1",
        project="fno",
        cwd="/tmp/abilities",
        _status="ready",
    )
    g.write_text(json.dumps({"entries": [epic]}) + "\n")

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")

    def read_entries():
        return json.loads(g.read_text())["entries"]

    return g, read_entries


def _groups_json(groups) -> str:
    return json.dumps(groups)


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

    by_frag = {e["plan_path"]: e for e in children}
    assert "internal/fno/plans/big.md#group-1" in by_frag
    assert "internal/fno/plans/big.md#group-2" in by_frag
    assert "internal/fno/plans/big.md#group-3" in by_frag

    # Each child parented to the epic with a #group fragment.
    for c in children:
        assert c["parent"] == "ab-epic0001"
        assert "#group-" in c["plan_path"]


def test_ac1_hp_inter_group_blocked_by_resolves_to_ids(graph_env):
    g, read_entries = graph_env
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])

    entries = read_entries()
    g1 = next(e for e in entries if e["plan_path"].endswith("#group-1"))
    g2 = next(e for e in entries if e["plan_path"].endswith("#group-2"))
    g3 = next(e for e in entries if e["plan_path"].endswith("#group-3"))

    assert g1["blocked_by"] == []
    assert g2["blocked_by"] == [g1["id"]]
    assert g3["blocked_by"] == [g2["id"]]


def test_wave_range_persisted_to_details(graph_env):
    g, read_entries = graph_env
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])
    g1 = next(e for e in read_entries() if e["plan_path"].endswith("#group-1"))
    # AC1-UI wave range is not just echoed - it persists on the child node.
    assert "1-3" in (g1.get("details") or "")


def test_inherits_epic_project_cwd(graph_env):
    g, read_entries = graph_env
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])
    children = [e for e in read_entries() if e.get("parent") == "ab-epic0001"]
    for c in children:
        assert c["project"] == "fno"
        assert c["cwd"] == "/tmp/abilities"


# -- per-group repo routing (multi-repo decomposition) --


def _patch_workmap(monkeypatch, mapping):
    """Stub project_root_from_settings so a known project resolves to a root."""
    import fno.graph._intake as intake

    monkeypatch.setattr(
        intake, "project_root_from_settings", lambda p: mapping.get(p)
    )


def test_per_group_project_derives_cwd_from_workmap(graph_env, monkeypatch):
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
    by_frag = {e["plan_path"]: e for e in read_entries() if e.get("parent") == "ab-epic0001"}
    g1 = by_frag["internal/fno/plans/big.md#group-1"]
    g2 = by_frag["internal/fno/plans/big.md#group-2"]
    # G1 inherits the epic's repo; G2 routed into web.
    assert (g1["project"], g1["cwd"]) == ("fno", "/tmp/abilities")
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
    g3 = next(e for e in entries if e["plan_path"].endswith("#group-3"))
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
    g3 = next(e for e in entries if e["plan_path"].endswith("#group-3"))
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
    g1 = next(e for e in read_entries() if e["plan_path"].endswith("#group-1"))
    assert "1-3" in (g1.get("details") or "")

    cleared = [{"slug": "1", "title": "G1", "waves": "", "blocked_by_groups": []}]
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(cleared)])
    g1 = next(e for e in read_entries() if e["plan_path"].endswith("#group-1"))
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
        _status="ready",
    )
    g.write_text(json.dumps({"entries": [epic]}) + "\n")

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")

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


def test_decompose_missing_doc_is_benign(graph_env):
    """A missing base doc must not fail decompose (it can never graduate early)."""
    # graph_env's epic plan_path points at a non-existent big.md.
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
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", tmp_path / "graph.json")
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
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
    DecomposeError,
    classify_group_dep,
    extract_contract_versions,
    validate_groups,
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

    child = next(e for e in read_entries() if e["plan_path"].endswith("#group-2"))
    assert child["dep"] == "contract"
    assert child["contract_version"] == 2
    assert child["stub_against"] == f"{doc}#interface-contract"
    # The hard sibling carries none of the contract fields (AC6-EDGE).
    sib = next(e for e in read_entries() if e["plan_path"].endswith("#group-1"))
    assert "dep" not in sib and "stub_against" not in sib and "contract_version" not in sib


def test_ac2_hp_contract_no_pin_downgrades_loudly(tmp_path, monkeypatch):
    # Doc has frontmatter but no ## Interface Contract section -> no pin.
    g, read_entries, doc = _contract_env(tmp_path, monkeypatch, body="# just a body\n")
    result = _invoke(
        ["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(_CONTRACT_GROUPS)]
    )
    assert result.exit_code == 0, result.output
    assert "falling back to hard" in result.output

    child = next(e for e in read_entries() if e["plan_path"].endswith("#group-2"))
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
    child = next(e for e in read_entries() if e["plan_path"].endswith("#group-2"))
    assert child["dep"] == "contract"

    # Re-decompose group 2 back to hard (drop the dep field).
    hard_again = [
        _CONTRACT_GROUPS[0],
        {**_CONTRACT_GROUPS[1], "dep": "hard"},
    ]
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(hard_again)])
    child = next(e for e in read_entries() if e["plan_path"].endswith("#group-2"))
    assert "dep" not in child
    assert "stub_against" not in child
    assert "contract_version" not in child


def test_ac6_edge_pure_hard_decompose_adds_no_contract_fields(graph_env):
    """A decomposition with only hard deps stamps no contract metadata."""
    g, read_entries = graph_env
    _invoke(["backlog", "decompose", "ab-epic0001", "--groups", _groups_json(THREE_GROUPS)])
    for child in (e for e in read_entries() if e.get("parent") == "ab-epic0001"):
        assert "dep" not in child
        assert "stub_against" not in child
        assert "contract_version" not in child
