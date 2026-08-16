"""fno autonomy status - x-aaaf wave 1 task 1.3.

Covers AC1-HP (every spawner appears with trigger/gate/value/rank) and
AC9-ERR (a repo with no fno config at all still prints defaults and exits 0).
"""
from __future__ import annotations

from pathlib import Path

import click.testing
import pytest
import typer.main
from typer.models import TyperInfo

from fno.autonomy_cli import (
    _UNGATED,
    SpawnerStatus,
    autonomy_app,
    collect_status,
    format_table,
)

runner = click.testing.CliRunner()


def _cli():
    """Build the same group shape production gets via get_group_from_info()
    (see fno._lazy_group): a bare typer.Typer with one command collapses
    into a top-level command under plain get_command(), which would silently
    change `fno autonomy status` into `fno autonomy`. Returns a plain
    click.Group, so it is invoked with click's own CliRunner (typer's
    CliRunner.invoke expects a typer.Typer, not an already-converted Group)."""
    return typer.main.get_group_from_info(
        TyperInfo(autonomy_app),
        pretty_exceptions_short=True,
        rich_markup_mode=None,
        suggest_commands=True,
    )


def _write_settings(tmp_path: Path, content: str) -> None:
    settings_dir = tmp_path / ".fno"
    settings_dir.mkdir(parents=True, exist_ok=True)
    (settings_dir / "settings.yaml").write_text(content, encoding="utf-8")


@pytest.fixture(autouse=True)
def _isolate_global_settings(monkeypatch: pytest.MonkeyPatch) -> None:
    """Never let the developer's real ~/.fno/settings.yaml leak into a
    load_settings_for_repo() read (config_io._global_settings_path)."""
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")


def test_ac1_hp_every_known_spawner_appears(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP: every spawner appears with trigger, gate key, value, and rank."""
    monkeypatch.delenv("FNO_AUTO_CONTINUE", raising=False)
    monkeypatch.delenv("FNO_THINK_SPAWN", raising=False)
    monkeypatch.delenv("FNO_KEEP_GOING", raising=False)
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / ".fno" / "settings.yaml"))
    _write_settings(tmp_path, "schema_version: 1\n")

    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    rows = collect_status(tmp_path)
    names = {r.name for r in rows}

    # 9 known spawners + 3 currently-ungated ones (AC1-HP + the "ungated
    # rather than omitted" requirement).
    assert len(rows) == 12
    for r in rows:
        assert r.trigger
        assert r.gate_key
        assert r.rank in {"env", "config", "default", "ungated"}


def test_ac1_hp_env_override_rank_is_visible(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """An overriding rank (env) is named on the row, never silent."""
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / ".fno" / "settings.yaml"))
    _write_settings(tmp_path, "schema_version: 1\n")
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    monkeypatch.setenv("FNO_AUTO_CONTINUE", "1")

    rows = collect_status(tmp_path)
    advance_row = next(r for r in rows if r.name == "advance (node-walk)")
    assert advance_row.armed is True
    assert advance_row.rank == "env"


def test_ungated_spawners_render_as_ungated_not_omitted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A spawner with no gate reads `ungated` rather than being omitted -
    omission is exactly what hid groom/restart/evals from the operator."""
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / ".fno" / "settings.yaml"))
    _write_settings(tmp_path, "schema_version: 1\n")
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    rows = collect_status(tmp_path)
    ungated_names = {name for name, _trigger in _UNGATED}
    found = {r.name for r in rows if r.rank == "ungated"}
    assert found == ungated_names
    for r in rows:
        if r.rank == "ungated":
            assert r.armed is None
            assert r.gate_key == "(none)"


def test_format_table_includes_every_row(tmp_path: Path) -> None:
    rows = [
        SpawnerStatus("a", "trigger-a", "config.a.enabled", True, "config"),
        SpawnerStatus("b", "trigger-b", "(none)", None, "ungated"),
    ]
    table = format_table(rows)
    assert "a" in table and "trigger-a" in table and "config.a.enabled" in table
    assert "true" in table
    assert "b" in table and "ungated" in table


def test_ac9_err_no_config_at_all_prints_defaults_exits_0(tmp_path: Path) -> None:
    """AC9-ERR: a repo with no fno config prints defaults rather than failing."""
    result = runner.invoke(_cli(), ["status", "--project-root", str(tmp_path)])
    assert result.exit_code == 0
    assert "SPAWNER" in result.stdout


def test_status_command_always_exits_0_even_on_resolver_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Introspection never gates: a broken resolver still exits 0 (AC9-ERR)."""
    import fno.autonomy_cli as autonomy_cli

    def _boom(_project_root):
        raise RuntimeError("settings exploded")

    monkeypatch.setattr(autonomy_cli, "collect_status", _boom)
    result = runner.invoke(_cli(), ["status", "--project-root", str(tmp_path)])
    assert result.exit_code == 0
