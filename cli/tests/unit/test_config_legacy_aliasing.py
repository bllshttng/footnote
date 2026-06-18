"""Legacy-key aliasing + fail-open regression tests.

Covers the codex P2 findings:
  - per-layer aliasing so legacy/canonical precedence holds across files
  - health_monitor / collision stay fail-open on a malformed UNRELATED setting
"""
from __future__ import annotations

from pathlib import Path

import pytest


def _write(p: Path, text: str) -> Path:
    p.write_text(text, encoding="utf-8")
    return p


def test_local_legacy_beats_global_canonical(tmp_path):
    """global canonical [gemini] + local legacy [codex] -> local wins ([codex]).

    Aliasing only the merged dict made `canonical_present` true from the global
    layer and dropped the higher-priority legacy value; per-layer aliasing fixes it.
    """
    from fno.config import settings_from_files

    glob = _write(
        tmp_path / "global.yaml",
        "config:\n  review:\n    external_reviewers:\n      - gemini\n",
    )
    local = _write(
        tmp_path / "local.yaml",
        "config:\n  external_reviewers:\n    - codex\n",
    )
    # highest-priority first (project beats user)
    s = settings_from_files([local, glob])
    assert s.config.review.external_reviewers == ["codex"]


def test_local_canonical_beats_global_legacy(tmp_path):
    from fno.config import settings_from_files

    glob = _write(
        tmp_path / "global.yaml",
        "config:\n  external_reviewers:\n    - gemini\n",
    )
    local = _write(
        tmp_path / "local.yaml",
        "config:\n  review:\n    external_reviewers:\n      - codex\n",
    )
    s = settings_from_files([local, glob])
    assert s.config.review.external_reviewers == ["codex"]


def test_legacy_scalar_aliases_to_list(tmp_path):
    from fno.config import settings_from_files

    f = _write(tmp_path / "s.yaml", "config:\n  external_reviewer: gemini\n")
    s = settings_from_files([f])
    assert s.config.review.external_reviewers == ["gemini"]


def test_top_level_project_aliases_id_and_vision(tmp_path):
    """The whole top-level project block (id + vision) lifts to config.project."""
    from fno.config import settings_from_files

    f = _write(tmp_path / "s.yaml", 'project:\n  id: myproj\n  vision: "ship it"\n')
    s = settings_from_files([f])
    assert s.config.project.id == "myproj"
    assert s.config.project.vision == "ship it"


def test_top_level_work_aliases_to_config_work(tmp_path):
    """Legacy top-level work: lifts to config.work."""
    from fno.config import settings_from_files

    f = _write(
        tmp_path / "s.yaml",
        "work:\n  workspaces:\n    main:\n      projects:\n"
        "      - name: web\n        path: ~/code/web\n",
    )
    s = settings_from_files([f])
    ws = s.config.work.workspaces.get("main")
    assert ws is not None and ws.projects[0].name == "web"


def test_canonical_config_work_wins_over_legacy_top_level(tmp_path):
    from fno.config import settings_from_files

    f = _write(
        tmp_path / "s.yaml",
        "work:\n  workspaces:\n    main:\n      projects:\n"
        "      - name: legacy\n        path: ~/x\n"
        "config:\n  work:\n    workspaces:\n      main:\n        projects:\n"
        "        - name: canonical\n          path: ~/y\n",
    )
    s = settings_from_files([f])
    assert s.config.work.workspaces["main"].projects[0].name == "canonical"


def test_health_load_config_fail_open_on_bad_unrelated_setting(tmp_path):
    """A malformed UNRELATED setting must not abort health checks (codex P2)."""
    from fno.health_monitor import DEFAULT_CONFIG, load_config

    # A reserved id_prefix fails model validation, but the health block itself
    # is fine; health must still resolve to defaults rather than abort.
    bad = _write(
        tmp_path / "s.yaml",
        "config:\n  backlog:\n    id_prefix: tgt\n  health_monitor:\n    enabled: true\n",
    )
    out = load_config(project_settings=bad, user_settings=tmp_path / "missing.yaml")
    assert out["enabled"] == DEFAULT_CONFIG["enabled"]
    assert out["thresholds"] == DEFAULT_CONFIG["thresholds"]


def test_collision_thresholds_fail_open_on_bad_unrelated_setting(tmp_path):
    from fno.graph.collision import DEFAULT_THRESHOLDS, _load_thresholds

    bad = _write(
        tmp_path / "s.yaml",
        "config:\n  backlog:\n    id_prefix: tgt\n  collision:\n    severity_thresholds:\n      high_count: 9\n",
    )
    out = _load_thresholds(project_settings=bad, user_settings=tmp_path / "missing.yaml")
    assert out == DEFAULT_THRESHOLDS


def test_max_iterations_degrades_instead_of_raising(tmp_path):
    """A non-positive max_iterations warns and falls back to 40 (gemini)."""
    from fno.config import settings_from_files

    f = _write(tmp_path / "s.yaml", "config:\n  target:\n    defaults:\n      max_iterations: 0\n")
    s = settings_from_files([f])
    assert s.config.target.defaults.max_iterations == 40
