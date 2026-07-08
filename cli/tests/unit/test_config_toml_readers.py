"""Regression: the bespoke settings readers must pick up a flat config.toml.

Post config-flatten (PR #269) an install has ONLY .fno/config.toml - the
migration deletes settings.yaml. These readers previously yaml.safe_load'd a
hardcoded settings.yaml path, so on a config.toml-only install they silently
returned defaults. Each now routes through config_read_candidates +
read_config_flat; these tests pin that a config.toml (no settings.yaml) is read.
"""
from __future__ import annotations

from pathlib import Path

import pytest


def _write_toml(tmp_path: Path, body: str) -> Path:
    fno = tmp_path / ".fno"
    fno.mkdir(parents=True, exist_ok=True)
    (fno / "config.toml").write_text(body, encoding="utf-8")
    return tmp_path


def test_v2_flag_reads_config_toml(tmp_path: Path) -> None:
    _write_toml(tmp_path, "v2_enabled = true\n")
    from fno.cli import _load_v2_config_flag

    assert _load_v2_config_flag(tmp_path) is True


def test_peer_surfaces_reads_config_toml(tmp_path: Path) -> None:
    _write_toml(tmp_path, '[inbox.peers.alice]\nsurfaces = ["api-server"]\n')
    from fno.inbox.settings import read_peer_surfaces

    assert read_peer_surfaces(tmp_path) == {"alice": ["api-server"]}


def test_config_toml_wins_over_legacy_settings_yaml(tmp_path: Path) -> None:
    """config.toml takes precedence when both files are present."""
    _write_toml(tmp_path, '[inbox.peers.alice]\nsurfaces = ["from-toml"]\n')
    (tmp_path / ".fno" / "settings.yaml").write_text(
        "config:\n  inbox:\n    peers:\n      alice:\n        surfaces: [from-yaml]\n",
        encoding="utf-8",
    )
    from fno.inbox.settings import read_peer_surfaces

    assert read_peer_surfaces(tmp_path) == {"alice": ["from-toml"]}


def test_triage_settings_reads_config_toml(tmp_path: Path) -> None:
    _write_toml(
        tmp_path, '[inbox.triage]\nmodel = "claude-opus-4-8"\ntimeout_sec = 99\n'
    )
    from fno.inbox.triage import read_triage_settings

    s = read_triage_settings(tmp_path)
    assert s.model == "claude-opus-4-8"
    assert s.timeout_sec == 99


def test_load_goals_reads_config_toml(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _write_toml(
        tmp_path,
        '[[project.goals]]\nid = "g1"\ngoal = "ship it"\nstatus = "active"\n',
    )
    monkeypatch.chdir(tmp_path)
    from fno.graph.triage import _load_goals

    assert _load_goals() == [{"id": "g1", "goal": "ship it", "status": "active"}]
