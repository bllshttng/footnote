"""EvalsBlock day keys: defaults, fail-safe degradation, config-get read.

The two keys (schedule_days, stale_days) are the demand side of the eval
bank: how often the pr-watch tick runs the regression tier, and the age at
which the newest regression run reads STALE. A malformed value must never
break load_settings(); it degrades to the modeled default with a WARNING,
the block's existing posture.
"""
from __future__ import annotations

import logging
from pathlib import Path

import pytest


def test_defaults() -> None:
    from fno.config import EvalsBlock

    b = EvalsBlock()
    assert b.enabled is True
    assert b.schedule_days == 7
    assert b.stale_days == 7


def test_bad_value_degrades_to_default_with_warning(caplog) -> None:
    from fno.config import EvalsBlock

    with caplog.at_level(logging.WARNING, logger="fno.config"):
        b = EvalsBlock(schedule_days="weekly", stale_days=None)
    assert b.schedule_days == 7
    assert b.stale_days == 7
    assert "schedule_days" in caplog.text
    assert "weekly" in caplog.text


def test_zero_schedule_disables_scheduled_run() -> None:
    from fno.config import EvalsBlock

    assert EvalsBlock(schedule_days=0).schedule_days == 0


def test_negative_degrades_to_default() -> None:
    from fno.config import EvalsBlock

    assert EvalsBlock(stale_days=-1).stale_days == 7


def test_bool_is_not_a_day_value() -> None:
    from fno.config import EvalsBlock

    # A bool is an int to Python; the sanitizer refuses it anyway.
    assert EvalsBlock(schedule_days=True).schedule_days == 7


def _run_get(key: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, body: str):
    f = tmp_path / ".fno" / "settings.yaml"
    f.parent.mkdir(parents=True, exist_ok=True)
    f.write_text(body, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(f))
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    from fno.cli import app

    from typer.testing import CliRunner

    return CliRunner().invoke(app, ["config", "get", key])


def test_config_get_schedule_days_default(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    r = _run_get(
        "config.evals.schedule_days", tmp_path, monkeypatch, "schema_version: 1\n"
    )
    assert r.exit_code == 0, r.output
    assert r.stdout.strip() == "7"


def test_config_get_stale_days_overridden(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    r = _run_get(
        "config.evals.stale_days",
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  evals:\n    stale_days: 3\n",
    )
    assert r.exit_code == 0, r.output
    assert r.stdout.strip() == "3"
