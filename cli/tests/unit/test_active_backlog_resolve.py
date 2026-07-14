"""Unit tests for active-backlog drain-target resolution (node x-c070).

resolve_drain_targets() turns config.active_backlog + the workspace project->path
map into the concrete drain targets the daemon shells for. It must be fully
fail-safe (a config or workspace fault yields no targets, never raises) and honor
the bool/per-project-map enable model.
"""
from __future__ import annotations

import fno.active_backlog as ab


def _patch(monkeypatch, *, enabled, interval="5m", failure_limit=3, mission=None, paths):
    from fno.config import ActiveBacklogConfig

    cfg = ActiveBacklogConfig(
        enabled=enabled, interval=interval, failure_limit=failure_limit, mission=mission
    )

    class _Settings:
        active_backlog = cfg

    monkeypatch.setattr(ab, "_workspace_paths", lambda: paths)
    # load_settings is imported inside resolve_drain_targets; patch at source.
    import fno.config as cfgmod

    monkeypatch.setattr(cfgmod, "load_settings", lambda: _Settings())
    # The same-project interval drain is retired by default (Locked Decision 2);
    # these tests exercise the underlying resolution logic behind the opt-in.
    monkeypatch.setenv("FNO_ACTIVE_BACKLOG_LEGACY_DRAIN", "1")
    return cfg


def test_disabled_yields_no_targets(monkeypatch):
    _patch(monkeypatch, enabled=False, paths={"footnote": "/repo/footnote"})
    assert ab.resolve_drain_targets() == []


def test_default_retires_same_project_interval_drain(monkeypatch):
    # Locked Decision 2 (x-0ad6): without the legacy opt-in, an ENABLED config
    # resolves to NO drain targets - the same-project interval drain is retired.
    # merge-triggered advance + manual dispatch are the same-project coverage.
    _patch(monkeypatch, enabled=True, paths={"footnote": "/repo/footnote"})
    monkeypatch.delenv("FNO_ACTIVE_BACKLOG_LEGACY_DRAIN", raising=False)
    assert ab.resolve_drain_targets() == []


def test_bool_true_drains_every_workspace_project(monkeypatch):
    _patch(
        monkeypatch,
        enabled=True,
        paths={"footnote": "/repo/footnote", "readyrule": "/repo/readyrule"},
    )
    targets = ab.resolve_drain_targets()
    assert [t.project for t in targets] == ["footnote", "readyrule"]
    assert all(t.interval_seconds == 300 for t in targets)
    assert {t.cwd for t in targets} == {"/repo/footnote", "/repo/readyrule"}


def test_bool_true_with_no_workspace_map_is_empty(monkeypatch):
    # No project to root a drain at -> no targets (fail-safe, never a guessed cwd).
    _patch(monkeypatch, enabled=True, paths={})
    assert ab.resolve_drain_targets() == []


def test_per_project_map_only_truthy_keys(monkeypatch):
    _patch(
        monkeypatch,
        enabled={"footnote": True, "readyrule": False},
        paths={"footnote": "/repo/footnote", "readyrule": "/repo/readyrule"},
    )
    targets = ab.resolve_drain_targets()
    assert [t.project for t in targets] == ["footnote"]
    assert targets[0].cwd == "/repo/footnote"


def test_enabled_project_without_workspace_path_is_skipped(monkeypatch):
    _patch(monkeypatch, enabled={"ghost": True}, paths={"footnote": "/repo/footnote"})
    assert ab.resolve_drain_targets() == []


def test_invalid_interval_disables_everything(monkeypatch):
    _patch(monkeypatch, enabled=True, interval="0s", paths={"footnote": "/repo/footnote"})
    assert ab.resolve_drain_targets() == []


def test_failure_limit_and_mission_propagate(monkeypatch):
    _patch(
        monkeypatch,
        enabled={"footnote": True},
        failure_limit=5,
        mission="fno-mission-7",
        paths={"footnote": "/repo/footnote"},
    )
    t = ab.resolve_drain_targets()[0]
    assert t.failure_limit == 5
    assert t.mission == "fno-mission-7"
    assert t.interval_seconds == 300


def test_load_settings_fault_yields_empty(monkeypatch):
    import fno.config as cfgmod

    def _boom():
        raise RuntimeError("settings exploded")

    monkeypatch.setattr(cfgmod, "load_settings", _boom)
    monkeypatch.setattr(ab, "_workspace_paths", lambda: {"footnote": "/repo/footnote"})
    assert ab.resolve_drain_targets() == []


def test_as_dicts_shape(monkeypatch):
    _patch(monkeypatch, enabled={"footnote": True}, paths={"footnote": "/repo/footnote"})
    dicts = ab.drain_targets_as_dicts()
    assert dicts == [
        {
            "project": "footnote",
            "cwd": "/repo/footnote",
            "interval_seconds": 300,
            "failure_limit": 3,
            "mission": None,
            # batch-lane (x-6cdf): per-repo config.batch.enabled; False for a
            # repo with no batch config (fail-safe default).
            "batch": False,
            # parallel mode (x-42d5 G4): per-repo config.parallel.max_lanes;
            # 1 (sequential) for a repo with no parallel config (fail-safe).
            "max_lanes": 1,
        }
    ]


def test_as_dicts_reads_configured_max_lanes(monkeypatch):
    # The happy path must actually reach config.parallel.max_lanes: if the
    # read path breaks (rename, loader change), the fail-safe 1 would silently
    # disable parallel mode and only this test notices.
    import fno.config as cfgmod

    class _Parallel:
        max_lanes = 3

    class _Settings:
        parallel = _Parallel()

    _patch(monkeypatch, enabled={"footnote": True}, paths={"footnote": "/repo/footnote"})
    monkeypatch.setattr(cfgmod, "load_settings_for_repo", lambda _p: _Settings())
    dicts = ab.drain_targets_as_dicts()
    assert dicts[0]["max_lanes"] == 3
