"""`fno config route settings ls`: the route-settings overlays, legibly.

The overlay files under state_dir()/route-settings are sha16-named and carry
live auth tokens, so before this verb the only way to answer "which file is
my session on, and is any of them stale" was a hand-written loop that opened
every file (x-5cc5). These tests pin the two properties the node asks for:
the WRONG case is disclosed by name (a stale tier renders old -> new), and
--prune removes only unreferenced old files.
"""
from __future__ import annotations

import json
import os
import time
from pathlib import Path
from types import SimpleNamespace

from typer.testing import CliRunner

from fno import paths
from fno.cli import app

runner = CliRunner()


def _overlay(dir_: Path, name: str, *, haiku: str, mtime_days_ago: float) -> Path:
    payload = {
        "env": {
            "ANTHROPIC_API_KEY": "",
            "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
            "ANTHROPIC_AUTH_TOKEN": "zk-secret",
            "ANTHROPIC_MODEL": "glm-5.3",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": haiku,
            "FNO_ROUTE_PROVIDER": "zai",
        }
    }
    path = dir_ / f"{name}.json"
    path.write_text(json.dumps(payload), encoding="utf-8")
    when = time.time() - mtime_days_ago * 86400.0
    os.utime(path, (when, when))
    return path


def _pin(tmp_path, monkeypatch, referenced: list[str]):
    """Point state_dir and the daemon-free registry read at the test."""
    base = tmp_path / "state"
    overlays = base / "route-settings"
    overlays.mkdir(parents=True, exist_ok=True)
    monkeypatch.setattr(paths, "state_dir", lambda: base)

    import fno.agents.registry as registry_mod

    rows = [
        SimpleNamespace(route_settings_path=p, name=f"row-{i}")
        for i, p in enumerate(referenced)
    ]
    monkeypatch.setattr(registry_mod, "load_registry", lambda: rows)
    return overlays


def test_stale_overlay_is_disclosed_by_name(tmp_path, monkeypatch):
    """The wrong case, named: an overlay still carrying glm-4.5-air renders a
    STALE marker naming glm-4.5-air -> glm-4.7, not a silent row."""
    overlays = _pin(tmp_path, monkeypatch, referenced=[])
    _overlay(overlays, "aa11", haiku="glm-4.5-air", mtime_days_ago=1)
    _overlay(overlays, "bb22", haiku="glm-4.7", mtime_days_ago=1)

    result = runner.invoke(app, ["config", "route", "settings", "ls"])

    assert result.exit_code == 0, result.output
    assert "aa11.json" in result.output and "zai" in result.output
    assert "glm-4.5-air -> glm-4.7" in result.output
    # Exactly one row carries the marker: a fresh file rendered stale (or a
    # stale file rendered fresh) would be its own lie.
    marker_lines = [line for line in result.output.splitlines() if "glm-4.5-air" in line]
    assert len(marker_lines) == 1
    assert "bb22.json" in result.output


def test_prune_removes_only_unreferenced_old_files_by_name(tmp_path, monkeypatch):
    """--prune deletes an unreferenced 20-day-old overlay and PRINTS its path;
    a referenced one survives regardless of age or staleness (pruning it would
    break a live row's recorded path)."""
    overlays = _pin(tmp_path, monkeypatch, referenced=[])
    old_unreferenced = _overlay(overlays, "cc44", haiku="glm-4.7", mtime_days_ago=20)
    young_unreferenced = _overlay(overlays, "dd55", haiku="glm-4.7", mtime_days_ago=2)
    old_referenced = _overlay(overlays, "ee66", haiku="glm-4.5-air", mtime_days_ago=20)
    _pin(tmp_path, monkeypatch, referenced=[str(old_referenced)])

    result = runner.invoke(
        app, ["config", "route", "settings", "ls", "--prune", "--age-days", "14"]
    )

    assert result.exit_code == 0, result.output
    assert not old_unreferenced.exists()
    assert f"pruned {old_unreferenced}" in result.output
    assert young_unreferenced.exists()
    assert old_referenced.exists()
    assert "1 file(s) pruned" in result.output


def test_json_rows_carry_the_staleness_facts(tmp_path, monkeypatch):
    overlays = _pin(tmp_path, monkeypatch, referenced=[])
    _overlay(overlays, "aa11", haiku="glm-4.5-air", mtime_days_ago=3)

    result = runner.invoke(app, ["config", "route", "settings", "ls", "--json"])

    assert result.exit_code == 0, result.output
    rows = json.loads(result.output)["rows"]
    assert rows[0]["file"] == "aa11.json"
    assert rows[0]["provider"] == "zai"
    assert rows[0]["haiku"] == "glm-4.5-air"
    assert rows[0]["stale"] == "glm-4.5-air -> glm-4.7"
    assert rows[0]["referenced"] == "no"


def test_prune_refuses_to_act_when_the_registry_cannot_be_read(tmp_path, monkeypatch):
    """Fail closed: with the registry unreadable, 'no row references it' is a
    guess, and deleting on a guess is the exact defect class this verb exists
    to refuse."""
    overlays = _pin(tmp_path, monkeypatch, referenced=[])
    old = _overlay(overlays, "cc44", haiku="glm-4.7", mtime_days_ago=20)
    import fno.agents.registry as registry_mod

    def _boom():
        raise RuntimeError("registry unreadable")

    monkeypatch.setattr(registry_mod, "load_registry", _boom)

    result = runner.invoke(
        app, ["config", "route", "settings", "ls", "--prune", "--age-days", "14"]
    )

    assert result.exit_code == 0, result.output
    assert old.exists()
    assert "?" in result.output  # the unknown-reference column is disclosed


def test_prune_refuses_an_age_floor_below_one_day(tmp_path, monkeypatch):
    """--age-days 0 would prune a file a launching spawn just wrote but has
    not yet recorded (unreferenced at age 0). Refused naming the minimum; the
    file is untouched."""
    overlays = _pin(tmp_path, monkeypatch, referenced=[])
    fresh = _overlay(overlays, "gg77", haiku="glm-4.7", mtime_days_ago=0.0)

    result = runner.invoke(
        app, ["config", "route", "settings", "ls", "--prune", "--age-days", "0"]
    )

    assert result.exit_code != 0
    assert "minimum is 1 day" in result.output
    assert fresh.exists()
