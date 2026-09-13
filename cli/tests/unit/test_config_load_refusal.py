"""`load_settings` refuses an out-of-schema value by name (x-49db).

The filed crash: one bad value in a config file raised the raw pydantic
ValidationError through every consumer (`fno backlog idea` died in a
traceback; the event mirror dropped every global mirror). The refusal names
the file, the key, the offending value and the legal set instead, and the
legacy flat shape must still name its file even though coercion derives a
deeper key than the file stores.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from pydantic import ValidationError

from fno.config import load_settings
from fno.config._loader import SettingsRefused


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_NO_CANONICAL_CONFIG", "1")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "global.toml"))
    monkeypatch.delenv("FNO_CONFIG", raising=False)


def _write(path: Path, body: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")
    return path


def test_nested_out_of_enum_value_refuses_by_name(tmp_path: Path) -> None:
    f = _write(
        tmp_path / ".fno" / "config.toml",
        'schema_version = 1\n[recovery.watchdog]\nmode = "on"\n',
    )
    with pytest.raises(SettingsRefused) as exc:
        load_settings()
    assert not isinstance(exc.value, ValidationError)
    msg = str(exc.value)
    assert str(f) in msg
    assert "recovery.watchdog.mode = 'on'" in msg
    assert "'report', 'wake' or 'handoff'" in msg


def test_legacy_flat_watchdog_string_refuses_and_names_its_file(tmp_path: Path) -> None:
    f = _write(
        tmp_path / ".fno" / "config.toml",
        'schema_version = 1\n[recovery]\nwatchdog = "on"\n',
    )
    with pytest.raises(SettingsRefused) as exc:
        load_settings()
    assert str(f) in str(exc.value)


def test_a_clean_config_still_loads(tmp_path: Path) -> None:
    _write(
        tmp_path / ".fno" / "config.toml",
        'schema_version = 1\n[recovery.watchdog]\nmode = "report"\n',
    )
    assert load_settings().recovery.watchdog.mode == "report"


def test_refusal_points_at_the_layer_that_decides(tmp_path: Path) -> None:
    """Global carries the bad value; the project file is clean and never sets
    the key; the refusal names the global file, the layer that decided it."""
    _write(
        tmp_path / ".fno" / "config.toml",
        'schema_version = 1\nstate_dir = "x"\n',
    )
    f = _write(
        tmp_path / "global.toml",
        'schema_version = 1\n[recovery.watchdog]\nmode = "on"\n',
    )
    with pytest.raises(SettingsRefused) as exc:
        load_settings()
    assert str(f) in str(exc.value)
