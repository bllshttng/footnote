"""Unit tests for active-backlog drain-target resolution (x-a4dc K2).

resolve_drain_targets() returns one DrainTarget per ACTIVE MISSION - an epic with
``mission_active=true`` - across all projects, gated by config.active_backlog +
resolved against the workspace project->path map. It must be fully fail-safe (a
config or graph fault yields no targets, never raises) and consult the graph, not
the retired per-project enable model.
"""
from __future__ import annotations

import fno.active_backlog as ab


def _patch(monkeypatch, *, enabled=True, interval="5m", failure_limit=3, missions, paths):
    """Wire a fake settings + active-mission set + workspace map.

    ``missions`` is the list of active-mission epic dicts _active_missions returns.
    """
    from fno.config import ActiveBacklogConfig

    cfg = ActiveBacklogConfig(enabled=enabled, interval=interval, failure_limit=failure_limit)

    class _Settings:
        active_backlog = cfg

    monkeypatch.setattr(ab, "_workspace_paths", lambda: paths)
    monkeypatch.setattr(ab, "_active_missions", lambda: missions)
    import fno.config as cfgmod

    # load_settings is imported inside resolve_drain_targets; patch at source.
    monkeypatch.setattr(cfgmod, "load_settings", lambda: _Settings())
    return cfg


def _mission(epic_id, project):
    return {"id": epic_id, "project": project, "mission_active": True}


def test_disabled_yields_no_targets(monkeypatch):
    _patch(
        monkeypatch,
        enabled=False,
        missions=[_mission("x-epic", "footnote")],
        paths={"footnote": "/repo/footnote"},
    )
    assert ab.resolve_drain_targets() == []


def test_no_active_missions_yields_no_targets(monkeypatch):
    # Enabled config, but nothing to drain: an enabled daemon with zero active
    # missions resolves to no targets (the mission is the unit of work now).
    _patch(monkeypatch, missions=[], paths={"footnote": "/repo/footnote"})
    assert ab.resolve_drain_targets() == []


def test_one_target_per_active_mission_in_id_order(monkeypatch):
    _patch(
        monkeypatch,
        missions=[_mission("x-bbb", "readyrule"), _mission("x-aaa", "footnote")],
        paths={"footnote": "/repo/footnote", "readyrule": "/repo/readyrule"},
    )
    targets = ab.resolve_drain_targets()
    # Sorted by epic id, one per mission, each carrying its epic on `mission`.
    assert [t.mission for t in targets] == ["x-aaa", "x-bbb"]
    assert [t.project for t in targets] == ["footnote", "readyrule"]
    assert [t.cwd for t in targets] == ["/repo/footnote", "/repo/readyrule"]
    assert all(t.interval_seconds == 300 for t in targets)


def test_mission_epic_without_workspace_path_is_skipped(monkeypatch):
    # No workspace cwd to root the loop -> skip that mission, keep the others.
    _patch(
        monkeypatch,
        missions=[_mission("x-ghost", "unmapped"), _mission("x-ok", "footnote")],
        paths={"footnote": "/repo/footnote"},
    )
    targets = ab.resolve_drain_targets()
    assert [t.mission for t in targets] == ["x-ok"]
    assert targets[0].cwd == "/repo/footnote"


def test_invalid_interval_disables_everything(monkeypatch):
    _patch(
        monkeypatch,
        interval="0s",
        missions=[_mission("x-epic", "footnote")],
        paths={"footnote": "/repo/footnote"},
    )
    assert ab.resolve_drain_targets() == []


def test_failure_limit_propagates(monkeypatch):
    _patch(
        monkeypatch,
        failure_limit=5,
        missions=[_mission("x-epic", "footnote")],
        paths={"footnote": "/repo/footnote"},
    )
    t = ab.resolve_drain_targets()[0]
    assert t.failure_limit == 5
    assert t.mission == "x-epic"
    assert t.interval_seconds == 300


def test_load_settings_fault_yields_empty(monkeypatch):
    import fno.config as cfgmod

    def _boom():
        raise RuntimeError("settings exploded")

    monkeypatch.setattr(cfgmod, "load_settings", _boom)
    monkeypatch.setattr(ab, "_active_missions", lambda: [_mission("x-epic", "footnote")])
    monkeypatch.setattr(ab, "_workspace_paths", lambda: {"footnote": "/repo/footnote"})
    assert ab.resolve_drain_targets() == []


def test_active_missions_read_fault_yields_empty(monkeypatch):
    # The real _active_missions must degrade to no missions on a graph read
    # fault, never propagate (the daemon stays alive on a corrupt/absent graph).
    import fno.graph.store as store

    def _boom(*_a, **_k):
        raise RuntimeError("graph exploded")

    monkeypatch.setattr(store, "read_graph", _boom)
    assert ab._active_missions() == []


def test_as_dicts_shape(monkeypatch):
    _patch(
        monkeypatch,
        missions=[_mission("x-epic", "footnote")],
        paths={"footnote": "/repo/footnote"},
    )
    dicts = ab.drain_targets_as_dicts()
    assert dicts == [
        {
            "project": "footnote",
            "cwd": "/repo/footnote",
            "interval_seconds": 300,
            "failure_limit": 3,
            "mission": "x-epic",
        }
    ]
