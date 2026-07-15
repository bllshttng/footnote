"""Tests for `fno providers register --config-dir` (x-d012).

The config-dir account is the verified-correct multi-account mechanism: a full
second login in its own dir that bills the right account. This registers a
record with config_dir set (no shared-slot snapshot).
"""
from __future__ import annotations

from pathlib import Path

import tomllib
from typer.testing import CliRunner

from fno.adapters.providers.cli import cli as providers_app

runner = CliRunner()


def _invoke(args, home: Path):
    return runner.invoke(
        providers_app, args, env={"HOME": str(home), "PWD": str(home)},
        catch_exceptions=False,
    )


def _records(home: Path) -> dict:
    data = tomllib.loads((home / ".fno" / "config.toml").read_text())
    return {r["id"]: r for r in data["providers"]["records"]}


def test_register_config_dir_writes_record(tmp_path):
    cfg = tmp_path / "claude-alt"
    cfg.mkdir()
    (cfg / ".credentials.json").write_text("{}")
    result = _invoke(["register", "readyrule", "--config-dir", str(cfg)], tmp_path)
    assert result.exit_code == 0, result.output
    rec = _records(tmp_path)["readyrule"]
    assert rec["config_dir"] == str(cfg)
    assert rec["auth"] == "managed"


def test_register_config_dir_missing_dir_refused(tmp_path):
    result = _invoke(
        ["register", "readyrule", "--config-dir", str(tmp_path / "nope")], tmp_path
    )
    assert result.exit_code == 1
    assert "does not exist" in result.output


def test_register_config_dir_relative_refused(tmp_path):
    result = _invoke(["register", "readyrule", "--config-dir", "rel/dir"], tmp_path)
    assert result.exit_code == 1
    assert "absolute" in result.output
