"""Unit tests for active-backlog drain-target resolution (x-a4dc K2, x-e221).

resolve_drain_targets() returns one DrainTarget per TERRITORY - a live crown
scope, plus one rung-1 territory per workspace project no live project-rung
crown rules - in scope order, gated by config.active_backlog. It must be fully
fail-safe (a config, registry, or graph fault yields no targets, never raises)
and the crown list seeds the mission list, not the reverse.
"""
from __future__ import annotations

import pytest

import fno.active_backlog as ab


def _patch(monkeypatch, *, enabled=True, interval="5m", failure_limit=3, crowns=(), epics=None, paths=None):
    """Wire a fake settings + live-crown set + workspace map.

    ``crowns`` is the list of (scope, level) tuples _live_crowns returns.
    ``epics`` maps epic id -> project for the _epic_project read.
    """
    from fno.config import ActiveBacklogConfig

    cfg = ActiveBacklogConfig(enabled=enabled, interval=interval, failure_limit=failure_limit)

    class _Settings:
        active_backlog = cfg

    monkeypatch.setattr(ab, "_workspace_paths", lambda **_: paths or {})
    monkeypatch.setattr(ab, "_live_crowns", lambda **_: [
        {"scope": scope, "level": level} for scope, level in crowns
    ])
    monkeypatch.setattr(ab, "_epic_project", lambda epic_id: (epics or {}).get(epic_id))
    import fno.config as cfgmod

    # load_settings is imported inside resolve_drain_targets; patch at source.
    monkeypatch.setattr(cfgmod, "load_settings", lambda: _Settings())
    return cfg


def test_disabled_yields_no_targets(monkeypatch):
    _patch(monkeypatch, enabled=False, crowns=[("x-epic", 2)], epics={"x-epic": "footnote"},
           paths={"footnote": "/repo/footnote"})
    assert ab.resolve_drain_targets() == []


def test_no_crowns_yields_one_kingless_territory_per_workspace(monkeypatch):
    # Enabled config, no crowns anywhere: every workspace project still drains
    # its loose nodes as a kingless rung-1 territory (machinery does not need
    # a king to dispatch).
    _patch(monkeypatch, crowns=[], paths={"footnote": "/repo/footnote"})
    targets = ab.resolve_drain_targets()
    assert len(targets) == 1
    t = targets[0]
    assert t.scope == "footnote"
    assert t.rung == 1
    assert t.kingless is True
    assert t.members == ("footnote",)
    assert t.mission is None


def test_one_target_per_crown_scope_in_scope_order(monkeypatch):
    _patch(
        monkeypatch,
        crowns=[("readyrule", 1), ("x-bbb", 2)],
        epics={"x-bbb": "footnote"},
        paths={"footnote": "/repo/footnote", "readyrule": "/repo/readyrule"},
    )
    targets = ab.resolve_drain_targets()
    # Sorted by scope; the rung-1 crown rules readyrule so readyrule gets no
    # extra kingless territory, while footnote's loose nodes still drain
    # (the rung-2 crown over x-bbb rules x-bbb's descendants, not footnote's
    # parentless nodes).
    assert [t.scope for t in targets] == ["footnote", "readyrule", "x-bbb"]
    assert [t.rung for t in targets] == [1, 1, 2]
    assert [t.kingless for t in targets] == [True, False, False]
    assert [t.mission for t in targets] == [None, None, "x-bbb"]
    assert targets[2].members == ("x-bbb",)


def test_multi_epic_crown_groups_members_on_one_target(monkeypatch):
    _patch(
        monkeypatch,
        crowns=[("x-a,x-b", 2)],
        epics={"x-a": "footnote", "x-b": "unmapped"},
        paths={"footnote": "/repo/footnote"},
    )
    targets = ab.resolve_drain_targets()
    # footnote's loose territory still exists beside the crowned one; the
    # crown is ONE target carrying both member epics.
    assert [t.scope for t in targets] == ["footnote", "x-a,x-b"]
    t = targets[1]
    assert t.members == ("x-a", "x-b")
    assert t.mission == "x-a"
    assert t.kingless is False
    # Rooted at the FIRST member epic's project; the converge core fans out
    # across projects at dispatch time.
    assert t.project == "footnote"
    assert t.cwd == "/repo/footnote"


def test_multi_epic_crown_without_workspace_root_is_skipped(monkeypatch):
    # No workspace cwd to root the first member epic -> skip that territory.
    _patch(
        monkeypatch,
        crowns=[("x-ghost,x-a", 2)],
        epics={"x-ghost": "unmapped", "x-a": "footnote"},
        paths={"footnote": "/repo/footnote"},
    )
    targets = ab.resolve_drain_targets()
    assert [t.scope for t in targets] == ["footnote"]  # the kingless loose one


def test_per_project_disabled_territory_is_skipped(monkeypatch):
    # enabled={proj: bool}: a territory rooted in an explicitly-disabled
    # project does not drain, even though any_enabled() is true for the daemon.
    _patch(
        monkeypatch,
        enabled={"footnote": True, "readyrule": False},
        crowns=[("readyrule", 1)],
        paths={"footnote": "/repo/footnote", "readyrule": "/repo/readyrule"},
    )
    targets = ab.resolve_drain_targets()
    assert [t.scope for t in targets] == ["footnote"]
    assert [t.project for t in targets] == ["footnote"]


def test_invalid_interval_disables_everything(monkeypatch):
    _patch(monkeypatch, interval="0s", crowns=[("x-epic", 2)],
           epics={"x-epic": "footnote"}, paths={"footnote": "/repo/footnote"})
    assert ab.resolve_drain_targets() == []


def test_failure_limit_propagates(monkeypatch):
    _patch(monkeypatch, failure_limit=5, crowns=[("x-epic", 2)],
           epics={"x-epic": "footnote"}, paths={"footnote": "/repo/footnote"})
    t = next(x for x in ab.resolve_drain_targets() if x.scope == "x-epic")
    assert t.failure_limit == 5
    assert t.scope == "x-epic"
    assert t.interval_seconds == 300


def test_load_settings_fault_yields_empty(monkeypatch):
    import fno.config as cfgmod

    def _boom():
        raise RuntimeError("settings exploded")

    monkeypatch.setattr(cfgmod, "load_settings", _boom)
    monkeypatch.setattr(ab, "_live_crowns", lambda **_: [])
    monkeypatch.setattr(ab, "_workspace_paths", lambda: {"footnote": "/repo/footnote"})
    assert ab.resolve_drain_targets() == []


def test_live_crowns_read_fault_yields_empty(monkeypatch):
    # The real _live_crowns must degrade to no crowns on a registry read
    # fault, never propagate (the daemon stays alive on an unreadable registry).
    import fno.agents.registry as registry

    def _boom(*_a, **_k):
        raise RuntimeError("registry exploded")

    monkeypatch.setattr(registry, "load_registry", _boom)
    assert ab._live_crowns() == []


def test_live_crowns_dedups_and_orders_canonical_scopes(monkeypatch):
    # Two live rows holding the same scope (one per harness) resolve to ONE
    # territory; scopes come back canonical-ordered.
    import fno.agents.registry as registry

    class _Row:
        def __init__(self, scope, level, status="live", name=""):
            self.crown_scope = scope
            self.crown_level = level
            self.status = status
            self.name = name

    monkeypatch.setattr(
        registry, "load_registry",
        lambda: [
            _Row("x-b", 2),
            _Row("x-a,x-b", 2),
            _Row("x-dead", 1, status="exited"),
        ],
    )
    crowns = ab._live_crowns()
    assert crowns == [
        {"scope": "x-a,x-b", "level": 2, "holder": ""},
        {"scope": "x-b", "level": 2, "holder": ""},
    ]


def test_strict_target_resolution_propagates_crown_read_fault(monkeypatch):
    _patch(monkeypatch, crowns=[], paths={"footnote": "/repo/footnote"})

    def _boom(*, strict=False):
        raise RuntimeError("crown read failed")

    monkeypatch.setattr(ab, "_live_crowns", _boom)
    with pytest.raises(RuntimeError, match="crown read failed"):
        ab.resolve_drain_targets(strict=True)


def test_as_dicts_shape(monkeypatch):
    _patch(monkeypatch, crowns=[("x-epic", 2)], epics={"x-epic": "footnote"},
           paths={"footnote": "/repo/footnote"})
    dicts = ab.drain_targets_as_dicts()
    crowned = next(d for d in dicts if d["scope"] == "x-epic")
    assert crowned == {
        "project": "footnote",
        "cwd": "/repo/footnote",
        "interval_seconds": 300,
        "failure_limit": 3,
        "mission": "x-epic",
        "scope": "x-epic",
        "rung": 2,
        "kingless": False,
        "members": ["x-epic"],
    }
