"""Unit tests for active-backlog drain-target resolution (x-a4dc K2).

resolve_drain_targets() returns one DrainTarget per ACTIVE MISSION - an epic with
``mission_active=true`` - across all projects, gated by config.active_backlog +
resolved against the workspace project->path map. It must be fully fail-safe (a
config or graph fault yields no targets, never raises) and consult the graph, not
the retired per-project enable model.
"""
from __future__ import annotations

import pytest

import fno.active_backlog as ab


def _patch(
    monkeypatch,
    *,
    enabled=True,
    interval="5m",
    failure_limit=3,
    max_concurrent=1,
    missions,
    paths,
):
    """Wire a fake settings + active-mission set + workspace map.

    ``missions`` is the list of active-mission epic dicts _active_missions returns.
    """
    from fno.config import ActiveBacklogConfig

    cfg = ActiveBacklogConfig(
        enabled=enabled,
        interval=interval,
        failure_limit=failure_limit,
        max_concurrent=max_concurrent,
    )

    class _Settings:
        active_backlog = cfg

    monkeypatch.setattr(ab, "_workspace_paths", lambda **_: paths)
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
    reading = ab.resolve_drain_reading()
    assert reading.skip_reason == "drain_disabled"
    # The acceptance: the disabled drain still counts the missions it is not
    # draining, so a reader cannot conclude the queue is empty (x-338c).
    assert reading.missions == 1


def test_no_active_missions_yields_no_targets(monkeypatch):
    # Enabled config, but nothing to drain: an enabled daemon with zero active
    # missions resolves to no targets (the mission is the unit of work now).
    _patch(monkeypatch, missions=[], paths={"footnote": "/repo/footnote"})
    assert ab.resolve_drain_targets() == []
    reading = ab.resolve_drain_reading()
    # The old word keeps its old meaning: no_missions only ever means an empty
    # queue, never a switched-off drain (AC7).
    assert reading.skip_reason == "no_missions"
    assert reading.missions == 0


def test_per_project_disabled_only_reports_project_disabled(monkeypatch):
    # The switch is on for the daemon (some project is enabled) but every
    # active mission lives in an explicitly-disabled project.
    _patch(
        monkeypatch,
        enabled={"third": True, "footnote": False, "readyrule": False},
        missions=[_mission("x-fno", "footnote"), _mission("x-rr", "readyrule")],
        paths={"footnote": "/repo/footnote", "readyrule": "/repo/readyrule"},
    )
    reading = ab.resolve_drain_reading()
    assert reading.targets == []
    assert reading.skip_reason == "project_disabled"
    assert reading.missions == 2


def test_missing_path_only_reports_no_workspace_path(monkeypatch):
    _patch(
        monkeypatch,
        missions=[_mission("x-ghost", "unmapped")],
        paths={},
    )
    reading = ab.resolve_drain_reading()
    assert reading.targets == []
    assert reading.skip_reason == "no_workspace_path"
    assert reading.missions == 1


def test_mixed_mission_drops_report_the_most_common(monkeypatch):
    _patch(
        monkeypatch,
        enabled={"third": True, "footnote": False, "readyrule": False},
        missions=[
            _mission("x-a", "footnote"),
            _mission("x-b", "readyrule"),
            _mission("x-c", "unmapped"),
        ],
        paths={"unmapped": None},
    )
    reading = ab.resolve_drain_reading()
    assert reading.targets == []
    # Two project_disabled drops against one missing path; the majority names
    # the reason (ties would fall to project_disabled by table order).
    assert reading.skip_reason == "project_disabled"
    assert reading.missions == 3


def test_invalid_interval_reports_bad_interval(monkeypatch):
    _patch(
        monkeypatch,
        interval="0s",
        missions=[_mission("x-epic", "footnote")],
        paths={"footnote": "/repo/footnote"},
    )
    reading = ab.resolve_drain_reading()
    assert reading.skip_reason == "bad_interval"
    assert reading.missions == 1


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


def test_per_project_disabled_mission_is_skipped(monkeypatch):
    # enabled={proj: bool}: a mission whose epic lives in an explicitly-disabled
    # project does not drain, even though any_enabled() is true for the daemon.
    _patch(
        monkeypatch,
        enabled={"footnote": True, "readyrule": False},
        missions=[_mission("x-fno", "footnote"), _mission("x-rr", "readyrule")],
        paths={"footnote": "/repo/footnote", "readyrule": "/repo/readyrule"},
    )
    targets = ab.resolve_drain_targets()
    assert [t.mission for t in targets] == ["x-fno"]
    assert [t.project for t in targets] == ["footnote"]


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
    reading = ab.resolve_drain_reading()
    assert reading.skip_reason == "config_unreadable"
    # The mission count survives the config fault: the graph is not the
    # config's to silence.
    assert reading.missions == 1


def test_quoted_boolean_enabled_warns_at_load():
    # "false" and false read identically in a readout and only one is a
    # boolean (x-338c): the load warns, then honors the value as written.
    from fno.config import ActiveBacklogConfig

    with pytest.warns(UserWarning, match="quoted string"):
        b = ActiveBacklogConfig(enabled="false")
    assert b.enabled is False
    with pytest.warns(UserWarning, match="quoted string"):
        b = ActiveBacklogConfig(enabled="true")
    assert b.enabled is True


def test_active_missions_read_fault_yields_empty(monkeypatch):
    # The real _active_missions must degrade to no missions on a graph read
    # fault, never propagate (the daemon stays alive on a corrupt/absent graph).
    import fno.graph.store as store

    def _boom(*_a, **_k):
        raise RuntimeError("graph exploded")

    monkeypatch.setattr(store, "read_graph", _boom)
    assert ab._active_missions() == []


def test_active_missions_non_list_graph_yields_empty(monkeypatch):
    # A malformed graph that read_graph returns as a non-iterable (e.g. None)
    # must degrade to no missions, never raise on the comprehension.
    import fno.graph.store as store

    monkeypatch.setattr(store, "read_graph", lambda *_a, **_k: None)
    assert ab._active_missions() == []


def test_strict_target_resolution_propagates_mission_read_fault(monkeypatch):
    _patch(monkeypatch, missions=[], paths={"footnote": "/repo/footnote"})

    def _boom(*, strict=False):
        raise RuntimeError("mission read failed")

    monkeypatch.setattr(ab, "_active_missions", _boom)
    with pytest.raises(RuntimeError, match="mission read failed"):
        ab.resolve_drain_targets(strict=True)


def test_as_dicts_shape(monkeypatch):
    _patch(
        monkeypatch,
        missions=[_mission("x-epic", "footnote")],
        paths={"footnote": "/repo/footnote"},
    )
    receipt = ab.drain_reading_as_dict()
    assert receipt == {
        "targets": [
            {
                "project": "footnote",
                "cwd": "/repo/footnote",
                "interval_seconds": 300,
                "failure_limit": 3,
                "mission": "x-epic",
                "max_concurrent": 1,
            }
        ],
        "missions": 1,
        "skip_reason": None,
    }


def test_as_dicts_receipt_names_a_disabled_drain(monkeypatch):
    """The JSON receipt the daemon reads carries the why beside the what."""
    _patch(
        monkeypatch,
        enabled=False,
        missions=[_mission("x-epic", "footnote")],
        paths={"footnote": "/repo/footnote"},
    )
    receipt = ab.drain_reading_as_dict()
    assert receipt["targets"] == []
    assert receipt["missions"] == 1
    assert receipt["skip_reason"] == "drain_disabled"


def test_max_concurrent_rides_on_every_target(monkeypatch):
    """The cap is global, so every target carries the same value.

    The daemon holds ONE gate for the whole drain; the target list is just its
    config channel. A cap that reached no target is a cap nothing enforces,
    which is how a declared 1 ran five concurrent converges.
    """
    _patch(
        monkeypatch,
        max_concurrent=3,
        missions=[_mission("x-a", "footnote"), _mission("x-b", "other")],
        paths={"footnote": "/repo/footnote", "other": "/repo/other"},
    )
    caps = [t.max_concurrent for t in ab.resolve_drain_targets()]
    assert caps == [3, 3]
