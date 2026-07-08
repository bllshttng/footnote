"""Tests for config.research - the `fno research` doc-deliverable config block.

The `doc` deliverable writes the brief + sources sidecar to
`config.research.output_dir`. That path is the vault area for research output
(e.g. ~/c3po/raw/readyrule) - NOT repo-relative, so absolute/tilde paths are
allowed (unlike post_merge.parking_lot_path). It is left unset by default so a
repo that has not opted in fails loud at the ship step rather than guessing a
landing path (the parking_lot_path lesson, AC5).
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner


def _write_settings(tmp_path: Path, content: str) -> Path:
    settings_dir = tmp_path / ".fno"
    settings_dir.mkdir(parents=True, exist_ok=True)
    settings_file = settings_dir / "settings.yaml"
    settings_file.write_text(content, encoding="utf-8")
    return settings_file


def _load(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, content: str):
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(tmp_path, content)
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    return config_mod.load_settings()


def _config_get(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, key: str, content: str):
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(tmp_path, content)
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    from fno.cli import app

    return CliRunner().invoke(app, ["config", "get", key])


def test_research_output_dir_defaults_none(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """No config.research block: output_dir is None so the ship step fails loud."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.research.output_dir is None


def test_research_output_dir_set_absolute(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """output_dir is read verbatim and may be absolute (a vault path, not repo-relative)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  research:\n"
        "    output_dir: ~/c3po/raw/readyrule\n",
    )
    assert settings.research.output_dir == "~/c3po/raw/readyrule"


def test_research_output_dir_rejects_glob(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A glob char in output_dir is rejected at load (mirrors path-field policy)."""
    with pytest.raises(Exception, match=r"\*|\?|\[|glob"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  research:\n"
            "    output_dir: '~/c3po/raw/*/out'\n",
        )


def test_config_get_resolves_output_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """`fno config get config.research.output_dir` returns the configured path."""
    result = _config_get(
        tmp_path,
        monkeypatch,
        "config.research.output_dir",
        "schema_version: 1\nconfig:\n  research:\n    output_dir: ~/c3po/raw/readyrule\n",
    )
    assert result.exit_code == 0, result.output
    assert result.output.strip() == "~/c3po/raw/readyrule"


def test_config_get_unset_output_dir_is_empty(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Unset output_dir prints empty (exit 0); the deliverable treats empty as unset."""
    result = _config_get(
        tmp_path, monkeypatch, "config.research.output_dir", "schema_version: 1\n"
    )
    assert result.exit_code == 0, result.output
    assert result.output.strip() == ""
