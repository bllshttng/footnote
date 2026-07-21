"""Unit tests for parallel-mode lane-fill selection (x-eb82, group 2).

Covers `advance.select_lane_fill`: distinct-domain selection, the cap, the
sequential-degrade edge, the recompute-after-each-claim contract (x-7441), the
skip-peer-held-lane guard, the domain-unset collapse, and the read-only preview
mode. `_ready_nodes` is monkeypatched so the selector's logic is tested without
shelling `fno backlog ready`; the claims root is isolated to `tmp_path`.
"""
from __future__ import annotations

from fno.backlog import advance
from fno.claims.lanes import acquire_lane_slot, active_lane_count, find_lane_slot


def _nodes(*specs):
    """Build ready-node summaries from (id, domain) pairs, in order."""
    return [{"id": i, "domain": d, "title": i} for i, d in specs]


def test_selects_first_of_each_distinct_domain_up_to_cap(tmp_path, monkeypatch):
    ready = _nodes(("n-a", "code"), ("n-b", "code"), ("n-c", "docs"), ("n-d", "infra"))
    monkeypatch.setattr(advance, "_ready_nodes", lambda project=None, mission=None: list(ready))

    sel = advance.select_lane_fill(3, claims_root=tmp_path)

    assert [n["id"] for n in sel] == ["n-a", "n-c", "n-d"]
    assert active_lane_count(root=tmp_path) == 3
    for n in sel:
        assert find_lane_slot(n["id"], root=tmp_path) is not None


def test_cap_limits_selection(tmp_path, monkeypatch):
    ready = _nodes(("n-a", "code"), ("n-c", "docs"), ("n-d", "infra"))
    monkeypatch.setattr(advance, "_ready_nodes", lambda project=None, mission=None: list(ready))

    sel = advance.select_lane_fill(2, claims_root=tmp_path)

    assert [n["id"] for n in sel] == ["n-a", "n-c"]
    assert active_lane_count(root=tmp_path) == 2


def test_max_lanes_one_selects_a_single_node(tmp_path, monkeypatch):
    """x-0ad6: max_lanes==1 selects a single ready node (the daemon's sequential
    fire-and-forget dispatch); max_lanes<1 still selects nothing."""
    ready = _nodes(("n-a", "code"), ("n-c", "docs"))
    monkeypatch.setattr(advance, "_ready_nodes", lambda project=None, mission=None: list(ready))

    assert advance.select_lane_fill(0, claims_root=tmp_path) == []
    assert active_lane_count(root=tmp_path) == 0

    sel = advance.select_lane_fill(1, claims_root=tmp_path)
    assert [s["id"] for s in sel] == ["n-a"]
    assert active_lane_count(root=tmp_path) == 1


def test_recomputes_distinctness_after_each_claim(tmp_path, monkeypatch):
    """x-7441: a fresh ready-list per pick, not a pre-claim snapshot.

    Pass 1 sees n-b(docs); before pass 2 a peer removes n-b from ready and n-c
    appears. The selector must pick n-c, never the stale n-b.
    """
    calls = {"n": 0}

    def fake_ready(project=None, mission=None):
        calls["n"] += 1
        if calls["n"] == 1:
            return _nodes(("n-a", "code"), ("n-b", "docs"))
        return _nodes(("n-a", "code"), ("n-c", "infra"))

    monkeypatch.setattr(advance, "_ready_nodes", fake_ready)

    sel = advance.select_lane_fill(3, claims_root=tmp_path)

    assert [n["id"] for n in sel] == ["n-a", "n-c"]
    assert calls["n"] >= 2, "must re-query ready between picks"


def test_seeds_used_domains_from_live_peer_lane_domains(tmp_path, monkeypatch):
    """codex P2: a live lane working `code` blocks selecting another `code` node.

    used_domains is seeded from the domains of live lane holders, so the
    distinct-domain guarantee holds across ticks (the fill-vacant-lanes case),
    not just within one call.
    """
    from fno.claims.lanes import acquire_lane_slot

    ready = _nodes(("n-b", "code"), ("n-c", "docs"))
    monkeypatch.setattr(advance, "_ready_nodes", lambda project=None, mission=None: list(ready))
    # A live peer lane already works a `code` node (records its domain).
    acquire_lane_slot(
        max_lanes=3, lane_id="n-a", extra_metadata={"domain": "code"}, root=tmp_path
    )

    sel = advance.select_lane_fill(3, claims_root=tmp_path)

    # `code` is covered by the live peer lane -> only `docs` is selectable.
    assert [n["id"] for n in sel] == ["n-c"]


def test_skips_node_a_peer_lane_already_holds(tmp_path, monkeypatch):
    """A node with a live lane slot is skipped so it is never double-dispatched."""
    ready = _nodes(("n-a", "code"), ("n-b", "docs"))
    monkeypatch.setattr(advance, "_ready_nodes", lambda project=None, mission=None: list(ready))
    # A peer lane already owns n-a (different domain would otherwise be picked).
    acquire_lane_slot(max_lanes=3, lane_id="n-a", root=tmp_path)

    sel = advance.select_lane_fill(3, claims_root=tmp_path)

    assert [n["id"] for n in sel] == ["n-b"]


def test_domain_unset_collapses_to_one_lane(tmp_path, monkeypatch):
    """Domain-less nodes share ONE bucket, never one lane each."""
    ready = [
        {"id": "n-a", "title": "n-a"},  # no domain
        {"id": "n-b", "domain": None, "title": "n-b"},  # explicit None
        {"id": "n-c", "domain": "docs", "title": "n-c"},
    ]
    monkeypatch.setattr(advance, "_ready_nodes", lambda project=None, mission=None: list(ready))

    sel = advance.select_lane_fill(3, claims_root=tmp_path)

    assert [n["id"] for n in sel] == ["n-a", "n-c"]


def test_preview_mode_holds_no_slots(tmp_path, monkeypatch):
    """claim=False returns the would-dispatch set without acquiring any slot."""
    ready = _nodes(("n-a", "code"), ("n-c", "docs"))
    monkeypatch.setattr(advance, "_ready_nodes", lambda project=None, mission=None: list(ready))

    sel = advance.select_lane_fill(3, claim=False, claims_root=tmp_path)

    assert [n["id"] for n in sel] == ["n-a", "n-c"]
    assert active_lane_count(root=tmp_path) == 0


def test_empty_ready_selects_nothing(tmp_path, monkeypatch):
    monkeypatch.setattr(advance, "_ready_nodes", lambda project=None, mission=None: [])
    assert advance.select_lane_fill(3, claims_root=tmp_path) == []


def test_midloop_raise_releases_already_acquired_slots(tmp_path, monkeypatch):
    """A raise on a LATER pick must not orphan slots from earlier picks.

    Pass 1 acquires n-a's slot; pass 2's ready query raises (a garbled
    `fno backlog ready`). The caller never receives `selected`, so select_lane_fill
    must release n-a's slot before re-raising (else it sits held until TTL).
    """
    import pytest

    from fno.claims.lanes import active_lane_count

    calls = {"n": 0}

    def flaky_ready(project=None, mission=None):
        calls["n"] += 1
        if calls["n"] == 1:
            return _nodes(("n-a", "code"), ("n-b", "docs"))
        raise RuntimeError("fno backlog ready exited 1: boom")

    monkeypatch.setattr(advance, "_ready_nodes", flaky_ready)

    with pytest.raises(RuntimeError, match="boom"):
        advance.select_lane_fill(3, claims_root=tmp_path)

    assert active_lane_count(root=tmp_path) == 0, "acquired slot must be released on raise"


# -- CLI seam: `fno backlog lane-fill` --

def test_cli_lane_fill_echoes_selection_json(monkeypatch):
    """The command echoes select_lane_fill's result as JSON and passes --claim."""
    import json

    from typer.testing import CliRunner

    from fno.graph import cli as gcli

    seen = {}

    def fake_select(max_lanes, project=None, *, mission=None, claim=False):
        seen.update(max_lanes=max_lanes, project=project, claim=claim)
        return [{"id": "n-a", "domain": "code"}]

    monkeypatch.setattr(advance, "select_lane_fill", fake_select)

    res = CliRunner().invoke(
        gcli.cli, ["lane-fill", "--max", "3", "--project", "fno", "--claim"]
    )

    assert res.exit_code == 0, res.stdout
    assert json.loads(res.stdout) == [{"id": "n-a", "domain": "code"}]
    assert seen == {"max_lanes": 3, "project": "fno", "claim": True}


def test_cli_lane_fill_defaults_max_from_config(monkeypatch):
    """With no --max, the command reads config.parallel.max_lanes."""
    from types import SimpleNamespace

    from typer.testing import CliRunner

    from fno.graph import cli as gcli

    seen = {}

    def fake_select(max_lanes, project=None, *, mission=None, claim=False):
        seen["max_lanes"] = max_lanes
        return []

    monkeypatch.setattr(advance, "select_lane_fill", fake_select)
    fake_settings = SimpleNamespace(
        parallel=SimpleNamespace(max_lanes=2)
    )
    monkeypatch.setattr("fno.config.load_settings", lambda: fake_settings)

    res = CliRunner().invoke(gcli.cli, ["lane-fill"])

    assert res.exit_code == 0, res.stdout
    assert seen["max_lanes"] == 2


def test_mission_scope_reaches_ready_query(tmp_path, monkeypatch):
    """codex P1 (PR #137): a mission-scoped caller's selection must query the
    ready list WITH the mission filter, mirroring MegawalkQueue::with_mission."""
    seen = {}

    def fake_ready(project=None, mission=None):
        seen["project"] = project
        seen["mission"] = mission
        return _nodes(("n-a", "code"))

    monkeypatch.setattr(advance, "_ready_nodes", fake_ready)
    sel = advance.select_lane_fill(2, "fno", mission="m-7", claims_root=tmp_path)
    assert [n["id"] for n in sel] == ["n-a"]
    assert seen == {"project": "fno", "mission": "m-7"}


def test_cli_ready_mission_filter(tmp_path, monkeypatch):
    """`fno backlog ready --mission` mirrors `next`'s mission_id filter."""
    import json

    from typer.testing import CliRunner

    from fno.graph import cli as gcli

    path = tmp_path / "graph.json"
    path.write_text(
        json.dumps(
            {
                "entries": [
                    {"id": "x-in", "title": "in", "status": "ready", "mission_id": "m-7"},
                    {"id": "x-out", "title": "out", "status": "ready"},
                ]
            }
        ),
        encoding="utf-8",
    )
    monkeypatch.setattr(gcli, "_graph_path", lambda: path)
    monkeypatch.setattr(gcli, "_live_claimed_node_ids", lambda: set())

    res = CliRunner().invoke(gcli.cli, ["ready", "--all", "--mission", "m-7"])
    assert res.exit_code == 0, res.output
    ids = [e["id"] for e in json.loads(res.output)]
    assert ids == ["x-in"]


# --- dispatch-time collision gate (x-2ada) -------------------------------


def _plan(tmp_path, name: str, files: list[str]) -> str:
    """Write a plan with a populated File Ownership Map; return its path."""
    rows = "\n".join(f"| `{f}` | modify | /blueprint |" for f in files)
    p = tmp_path / f"{name}.md"
    p.write_text(
        f"# {name}\n\n## File Ownership Map\n\n"
        f"| File | Action | Owner |\n|---|---|---|\n{rows}\n"
    )
    return str(p)


def test_skips_node_colliding_with_a_pick_from_this_round(tmp_path, monkeypatch):
    """Two ready nodes on the same file surface: only the first dispatches.

    The x-2ada incident: three nodes on one root cause, all landing in the same
    file, fired in parallel because nothing consulted the collision check.
    """
    shared = ["cli/src/fno/graph/cli.py", "cli/src/fno/graph/store.py"]
    ready = [
        {"id": "n-a", "domain": "code", "title": "a", "plan_path": _plan(tmp_path, "a", shared)},
        {"id": "n-b", "domain": "docs", "title": "b", "plan_path": _plan(tmp_path, "b", shared)},
        {"id": "n-c", "domain": "infra", "title": "c",
         "plan_path": _plan(tmp_path, "c", ["docs/unrelated.md"])},
    ]
    monkeypatch.setattr(advance, "_ready_nodes", lambda project=None, mission=None: list(ready))

    sel = advance.select_lane_fill(3, claims_root=tmp_path)

    assert [n["id"] for n in sel] == ["n-a", "n-c"]
    assert find_lane_slot("n-b", root=tmp_path) is None  # left ready, not parked


def test_collision_gate_fails_open_on_error(tmp_path, monkeypatch):
    """An unreadable collision surface must let dispatch proceed, not wedge it."""
    shared = ["cli/src/fno/graph/cli.py", "cli/src/fno/graph/store.py"]
    ready = [
        {"id": "n-a", "domain": "code", "title": "a", "plan_path": _plan(tmp_path, "a", shared)},
        {"id": "n-b", "domain": "docs", "title": "b", "plan_path": _plan(tmp_path, "b", shared)},
    ]
    monkeypatch.setattr(advance, "_ready_nodes", lambda project=None, mission=None: list(ready))

    def boom(*a, **k):
        raise OSError("collision surface unreadable")

    monkeypatch.setattr("fno.graph.collision.find_collisions", boom)

    sel = advance.select_lane_fill(2, claims_root=tmp_path)

    assert [n["id"] for n in sel] == ["n-a", "n-b"]


def test_node_without_plan_path_dispatches(tmp_path, monkeypatch):
    """No plan means no surface to compare; the gate must not block it."""
    ready = [{"id": "n-a", "domain": "code", "title": "a"}]
    monkeypatch.setattr(advance, "_ready_nodes", lambda project=None, mission=None: list(ready))

    assert [n["id"] for n in advance.select_lane_fill(1, claims_root=tmp_path)] == ["n-a"]
