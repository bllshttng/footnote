"""Tests for the epic kickoff / converge verb (x-9608 K1).

Covers the mission fan-out (kickoff_epic), the shared converge core reuse, the
mission-activation graph field, per-project + overall caps, per-child isolation,
idempotence (not TTL-dependent), and cascade-close deactivation.

Claim + graph isolation mirrors test_advance: claims route under a tmp
FNO_CLAIMS_ROOT/FNO_REPO_ROOT, the graph is a tmp graph.json with fno.paths.graph_json
patched to it, and _spawn_worker / _ready_leaf_children are patched at the module.
"""
from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.backlog import advance as adv
from fno.claims.core import acquire_claim, claim_status


@pytest.fixture
def iso(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_AUTO_CONTINUE", "1")  # armed by default
    return tmp_path / ".fno" / "events.jsonl"


def _events(p: Path) -> list[dict]:
    if not p.exists():
        return []
    return [json.loads(line) for line in p.read_text().splitlines() if line.strip()]


def _write_graph(tmp_path: Path, entries: list[dict], monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": entries}) + "\n")
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


def _epic_graph(tmp_path, monkeypatch, *, children_done=False):
    """Epic x-EPIC with two leaf children in two projects (web, etl)."""
    done = {"completed_at": "2026-07-18T00:00:00Z"} if children_done else {}
    entries = [
        {"id": "x-EPIC", "title": "mission", "type": "epic", "project": "fno"},
        {"id": "x-web", "title": "web child", "parent": "x-EPIC", "project": "web",
         "slug": "web-child", "_status": "ready", **done},
        {"id": "x-etl", "title": "etl child", "parent": "x-EPIC", "project": "etl",
         "slug": "etl-child", "_status": "ready", **done},
    ]
    _write_graph(tmp_path, entries, monkeypatch)
    return entries


def _patch_map(monkeypatch, mapping: dict[str, str]):
    """project name -> work-map root; unmapped -> None."""
    monkeypatch.setattr(
        "fno.graph._intake.project_root_from_settings",
        lambda project: mapping.get(project),
    )


def _patch_max_lanes(monkeypatch, n: int):
    monkeypatch.setattr(adv, "_max_lanes", lambda: n)


def _read_epic(epic_id="x-EPIC"):
    """Read the epic node via the (test-patched) graph_json, resolved at call time."""
    import fno.paths as _p
    from fno.graph.store import read_graph
    from fno.graph._intake import _find_node

    return _find_node(read_graph(_p.graph_json()), epic_id)


def _patch_spawn(monkeypatch, *, claim_node=True, fail_on=None):
    """Fake _spawn_worker collecting calls.

    ``claim_node`` acquires the real node:<id> claim (as a live worker's target
    init would) so re-run idempotence is exercised against real liveness, not a
    stub. ``fail_on`` (a node id) raises to model a spawn failure for that child.
    """
    calls = []

    def fake(node_id, root, slug=None, *, model=None, provider=None,
             verb=None, brief=None, **kw):
        if fail_on is not None and node_id == fail_on:
            raise adv.SpawnError("spawn boom")
        calls.append({"node": node_id, "root": root, "slug": slug})
        if claim_node:
            acquire_claim(f"node:{node_id}", f"worker:{node_id}", ttl_ms=60_000,
                          root=adv._claims_root_for(f"node:{node_id}"))
        return "short-" + node_id[-4:]

    monkeypatch.setattr(adv, "_spawn_worker", fake)
    return calls


def _ready(*ids_projects):
    """Build a _ready_leaf_children return list from (id, project) pairs."""
    return [
        {"id": nid, "slug": nid, "title": nid, "project": proj}
        for nid, proj in ids_projects
    ]


# ---------------------------------------------------------------------------
# Refusals
# ---------------------------------------------------------------------------


def test_refuse_non_container(iso, tmp_path, monkeypatch):
    """A leaf node named to --epic is refused by name (AC: refuse non-container)."""
    _write_graph(tmp_path, [{"id": "x-leaf", "title": "leaf", "project": "fno"}], monkeypatch)
    res = adv.kickoff_epic("x-leaf", events_path=iso)
    assert res.error == "not-a-container"
    assert res.dispatched == () and _events(iso) == []


def test_refuse_no_such_node(iso, tmp_path, monkeypatch):
    _write_graph(tmp_path, [{"id": "x-EPIC", "parent": None}], monkeypatch)
    res = adv.kickoff_epic("x-nope", events_path=iso)
    assert res.error == "no-such-node"


def test_disabled_dispatches_nothing(iso, tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_AUTO_CONTINUE", "0")
    _epic_graph(tmp_path, monkeypatch)
    monkeypatch.setattr(adv, "_ready_leaf_children",
                        lambda e: pytest.fail("must not enumerate"))
    res = adv.kickoff_epic("x-EPIC", events_path=iso)
    assert res.error == "disabled"


# ---------------------------------------------------------------------------
# Happy path (AC1-HP)
# ---------------------------------------------------------------------------


def test_fans_out_one_per_mapped_project(iso, tmp_path, monkeypatch):
    """AC1-HP: kickoff dispatches exactly one worker per ready child in each mapped project."""
    _epic_graph(tmp_path, monkeypatch)
    _patch_map(monkeypatch, {"web": str(tmp_path / "web"), "etl": str(tmp_path / "etl")})
    _patch_max_lanes(monkeypatch, 4)
    calls = _patch_spawn(monkeypatch)
    monkeypatch.setattr(adv, "_ready_leaf_children",
                        lambda e: _ready(("x-web", "web"), ("x-etl", "etl")))

    res = adv.kickoff_epic("x-EPIC", events_path=iso)

    assert res.activated is True
    assert set(res.dispatched) == {"x-web", "x-etl"}
    assert {c["node"] for c in calls} == {"x-web", "x-etl"}
    # each launched with its OWN work-map root
    roots = {c["node"]: c["root"] for c in calls}
    assert roots["x-web"] == str(tmp_path / "web")
    assert roots["x-etl"] == str(tmp_path / "etl")
    evs = _events(iso)
    assert [e for e in evs if e["type"] == "mission_activated"]
    disp = [e for e in evs if e["type"] == "advance_dispatched"]
    assert len(disp) == 2
    assert all(e["data"]["mission"] == "x-EPIC" for e in disp)
    assert all(e["data"]["cross_project"] is True for e in disp)


def test_mission_active_field_set(iso, tmp_path, monkeypatch):
    _epic_graph(tmp_path, monkeypatch)
    _patch_map(monkeypatch, {"web": str(tmp_path / "web"), "etl": str(tmp_path / "etl")})
    _patch_max_lanes(monkeypatch, 4)
    _patch_spawn(monkeypatch)
    monkeypatch.setattr(adv, "_ready_leaf_children",
                        lambda e: _ready(("x-web", "web"), ("x-etl", "etl")))

    adv.kickoff_epic("x-EPIC", events_path=iso)

    assert _read_epic().get("mission_active") is True


# ---------------------------------------------------------------------------
# Per-child isolation (AC1-ERR / AC2-ERR)
# ---------------------------------------------------------------------------


def test_unmapped_project_loud_skip_others_dispatch(iso, tmp_path, monkeypatch):
    """AC1-ERR: an unmapped project -> loud skip naming the config key; others still dispatch."""
    _epic_graph(tmp_path, monkeypatch)
    _patch_map(monkeypatch, {"web": str(tmp_path / "web")})  # etl unmapped
    _patch_max_lanes(monkeypatch, 4)
    calls = _patch_spawn(monkeypatch)
    monkeypatch.setattr(adv, "_ready_leaf_children",
                        lambda e: _ready(("x-web", "web"), ("x-etl", "etl")))

    res = adv.kickoff_epic("x-EPIC", events_path=iso)

    assert res.dispatched == ("x-web",)
    skips = [e for e in _events(iso) if e["type"] == "advance_skipped"]
    unmapped = [e for e in skips if e["data"]["reason"] == "unmapped-project"]
    assert len(unmapped) == 1
    assert unmapped[0]["data"]["node_id"] == "x-etl"
    assert "etl" in unmapped[0]["data"]["detail"]
    assert "config.work.workspaces" in unmapped[0]["data"]["detail"]


def test_spawn_failure_isolated_reservation_released(iso, tmp_path, monkeypatch):
    """AC2-ERR: a spawn failure -> failed receipt, no stale node:<id>, reservation released, others dispatch."""
    _epic_graph(tmp_path, monkeypatch)
    _patch_map(monkeypatch, {"web": str(tmp_path / "web"), "etl": str(tmp_path / "etl")})
    _patch_max_lanes(monkeypatch, 4)
    calls = _patch_spawn(monkeypatch, fail_on="x-web")
    monkeypatch.setattr(adv, "_ready_leaf_children",
                        lambda e: _ready(("x-web", "web"), ("x-etl", "etl")))

    res = adv.kickoff_epic("x-EPIC", events_path=iso)

    # etl still dispatched despite web failing
    assert res.dispatched == ("x-etl",)
    failed = [r for r in res.child_results if r.decision == "failed"]
    assert [r.node_id for r in failed] == ["x-web"]
    # no stale node:<id> for the failed child, and its dispatch reservation released
    assert claim_status(f"node:x-web",
                        root=adv._claims_root_for("node:x-web")).get("state") != "live"
    assert claim_status(f"dispatch:x-web",
                        root=adv._claims_root_for("dispatch:x-web")).get("state") != "live"
    evs = _events(iso)
    fe = [e for e in evs if e["type"] == "advance_failed"]
    assert len(fe) == 1 and fe[0]["data"]["mission"] == "x-EPIC"


# ---------------------------------------------------------------------------
# Idempotence (AC1-EDGE / AC2-FR)
# ---------------------------------------------------------------------------


def test_rerun_dispatches_nothing_already_claimed(iso, tmp_path, monkeypatch):
    """AC1-EDGE: an immediate re-run dispatches nothing already node:<id>-claimed."""
    _epic_graph(tmp_path, monkeypatch)
    _patch_map(monkeypatch, {"web": str(tmp_path / "web"), "etl": str(tmp_path / "etl")})
    _patch_max_lanes(monkeypatch, 4)
    calls = _patch_spawn(monkeypatch)  # claims node:<id> like a real worker
    monkeypatch.setattr(adv, "_ready_leaf_children",
                        lambda e: _ready(("x-web", "web"), ("x-etl", "etl")))

    first = adv.kickoff_epic("x-EPIC", events_path=iso)
    assert set(first.dispatched) == {"x-web", "x-etl"}
    calls.clear()

    second = adv.kickoff_epic("x-EPIC", events_path=iso)
    assert second.dispatched == ()  # both children still claimed
    assert calls == []
    # every second-pass child result is a skip (already-claimed)
    assert all(r.decision == "skipped" for r in second.child_results)


def test_idempotence_not_ttl_dependent(iso, tmp_path, monkeypatch):
    """AC2-FR: kill mid-fanout, re-run dispatches only the remainder, once each.

    First pass dispatches web (holds node:web) but etl's spawn fails (no claim
    left). Second pass: web is skipped (still claimed), etl dispatches. Dedup is
    on node:<id> liveness, NOT the dispatch TTL still being live.
    """
    _epic_graph(tmp_path, monkeypatch)
    _patch_map(monkeypatch, {"web": str(tmp_path / "web"), "etl": str(tmp_path / "etl")})
    _patch_max_lanes(monkeypatch, 4)
    calls = _patch_spawn(monkeypatch, fail_on="x-etl")
    monkeypatch.setattr(adv, "_ready_leaf_children",
                        lambda e: _ready(("x-web", "web"), ("x-etl", "etl")))

    first = adv.kickoff_epic("x-EPIC", events_path=iso)
    assert first.dispatched == ("x-web",)
    calls.clear()

    # etl's failed reservation must have been released (no live dispatch:<id>) so
    # the second pass can re-dispatch it WITHOUT waiting on a TTL.
    assert claim_status("dispatch:x-etl",
                        root=adv._claims_root_for("dispatch:x-etl")).get("state") != "live"

    _patch_spawn(monkeypatch)  # etl now succeeds on retry
    second = adv.kickoff_epic("x-EPIC", events_path=iso)
    assert second.dispatched == ("x-etl",)  # web skipped (claimed), etl dispatched


# ---------------------------------------------------------------------------
# All-done epic (AC2-EDGE)
# ---------------------------------------------------------------------------


def test_all_done_epic_noop_deactivates(iso, tmp_path, monkeypatch):
    """AC2-EDGE: kickoff on an all-done epic -> no dispatch, mission deactivated."""
    _epic_graph(tmp_path, monkeypatch, children_done=True)
    _patch_map(monkeypatch, {"web": str(tmp_path / "web"), "etl": str(tmp_path / "etl")})
    monkeypatch.setattr(adv, "_ready_leaf_children",
                        lambda e: pytest.fail("must not enumerate an all-done epic"))
    monkeypatch.setattr(adv, "_spawn_worker",
                        lambda *a, **k: pytest.fail("must not spawn"))

    res = adv.kickoff_epic("x-EPIC", events_path=iso)

    assert res.deactivated is True and res.all_done is True
    assert res.dispatched == ()
    dm = [e for e in _events(iso) if e["type"] == "mission_deactivated"]
    assert len(dm) == 1 and dm[0]["data"]["reason"] == "complete"


# ---------------------------------------------------------------------------
# Stop
# ---------------------------------------------------------------------------


def test_stop_deactivates_mission(iso, tmp_path, monkeypatch):
    entries = _epic_graph(tmp_path, monkeypatch)
    # pre-mark active
    entries[0]["mission_active"] = True
    _write_graph(tmp_path, entries, monkeypatch)
    monkeypatch.setattr(adv, "_spawn_worker",
                        lambda *a, **k: pytest.fail("must not spawn on stop"))

    res = adv.kickoff_epic("x-EPIC", stop=True, events_path=iso)

    assert res.deactivated is True
    assert "mission_active" not in _read_epic()
    dm = [e for e in _events(iso) if e["type"] == "mission_deactivated"]
    assert dm and dm[0]["data"]["reason"] == "stop"


# ---------------------------------------------------------------------------
# Caps
# ---------------------------------------------------------------------------


def test_overall_max_caps_dispatch(iso, tmp_path, monkeypatch):
    """--max caps total dispatches this pass."""
    _epic_graph(tmp_path, monkeypatch)
    _patch_map(monkeypatch, {"web": str(tmp_path / "web"), "etl": str(tmp_path / "etl")})
    _patch_max_lanes(monkeypatch, 4)
    calls = _patch_spawn(monkeypatch)
    monkeypatch.setattr(adv, "_ready_leaf_children",
                        lambda e: _ready(("x-web", "web"), ("x-etl", "etl")))

    res = adv.kickoff_epic("x-EPIC", max_dispatch=1, events_path=iso)

    assert len(res.dispatched) == 1
    assert len(calls) == 1


def test_per_project_lane_cap(iso, tmp_path, monkeypatch):
    """config.parallel.max_lanes bounds per-project concurrency (both children same project)."""
    entries = [
        {"id": "x-EPIC", "title": "mission", "project": "fno"},
        {"id": "x-a", "parent": "x-EPIC", "project": "web", "slug": "a", "_status": "ready"},
        {"id": "x-b", "parent": "x-EPIC", "project": "web", "slug": "b", "_status": "ready"},
    ]
    _write_graph(tmp_path, entries, monkeypatch)
    _patch_map(monkeypatch, {"web": str(tmp_path / "web")})
    _patch_max_lanes(monkeypatch, 1)
    calls = _patch_spawn(monkeypatch)
    monkeypatch.setattr(adv, "_ready_leaf_children",
                        lambda e: _ready(("x-a", "web"), ("x-b", "web")))

    res = adv.kickoff_epic("x-EPIC", events_path=iso)

    assert len(res.dispatched) == 1  # max_lanes=1 for project web
    lane_caps = [e for e in _events(iso)
                 if e["type"] == "advance_skipped" and e["data"]["reason"] == "lane-cap"]
    assert len(lane_caps) == 1


def test_lane_cap_seeds_live_workers(iso, tmp_path, monkeypatch):
    """A project already at max_lanes (a live worker) dispatches zero more."""
    _epic_graph(tmp_path, monkeypatch)
    _patch_map(monkeypatch, {"web": str(tmp_path / "web"), "etl": str(tmp_path / "etl")})
    _patch_max_lanes(monkeypatch, 1)
    # web already has one live worker
    monkeypatch.setattr(adv, "_live_workers_by_project", lambda: {"web": 1})
    calls = _patch_spawn(monkeypatch)
    monkeypatch.setattr(adv, "_ready_leaf_children",
                        lambda e: _ready(("x-web", "web"), ("x-etl", "etl")))

    res = adv.kickoff_epic("x-EPIC", events_path=iso)

    # web is capped by the pre-existing live worker; only etl dispatches
    assert res.dispatched == ("x-etl",)


# ---------------------------------------------------------------------------
# Cascade-close deactivation
# ---------------------------------------------------------------------------


def test_cascade_close_clears_mission_active(monkeypatch):
    """_cascade_close_parents clears mission_active when the epic auto-closes."""
    from fno.graph.cli import _cascade_close_parents

    entries = [
        {"id": "x-EPIC", "project": "fno", "mission_active": True},
        {"id": "x-only", "parent": "x-EPIC", "project": "web",
         "completed_at": "2026-07-18T00:00:00Z"},
    ]
    closed = _cascade_close_parents(entries, "x-only")
    assert "x-EPIC" in closed
    epic = next(e for e in entries if e["id"] == "x-EPIC")
    assert "mission_active" not in epic


# ---------------------------------------------------------------------------
# CLI wiring
# ---------------------------------------------------------------------------


def _cli_output(r) -> str:
    """stdout + stderr, tolerant of Click's mix_stderr version differences."""
    out = r.stdout or ""
    try:
        out += r.stderr or ""
    except (ValueError, AttributeError):
        pass  # stderr not separately captured on this Click version
    return out


def test_cli_epic_and_closed_mutually_exclusive(iso, tmp_path, monkeypatch):
    from typer.testing import CliRunner
    from fno.cli import app

    r = CliRunner().invoke(app, ["backlog", "advance", "--epic", "x-EPIC", "--closed", "x-1"])
    assert r.exit_code == 2
    assert "mutually exclusive" in _cli_output(r)


def test_cli_stop_requires_epic(iso, tmp_path, monkeypatch):
    from typer.testing import CliRunner
    from fno.cli import app

    r = CliRunner().invoke(app, ["backlog", "advance", "--stop"])
    assert r.exit_code == 2
    assert "require --epic" in _cli_output(r)
