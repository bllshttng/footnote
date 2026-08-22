"""fno agents autonomy status - x-aaaf wave 1 task 1.3.

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
    change `fno agents autonomy status` into `fno agents autonomy`. Returns a plain
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

    # master switch + 9 known spawners + groom/restart/evals (wave 2 gated
    # these) + recovery sweep (found while building the wave-3 registry ratchet)
    # + the king loop.
    assert len(rows) == 15
    for r in rows:
        assert r.trigger
        assert r.gate_key
        assert r.rank in {"env", "config", "default", "autonomy"}


def test_first_run_surfaces_name_autonomy_status_and_default_merge_posture() -> None:
    readme = Path(__file__).parents[2] / "README.md"
    text = readme.read_text(encoding="utf-8")

    from fno.config import SettingsModel

    assert SettingsModel().auto_merge.enabled is False
    assert "fno agents autonomy status" in text
    assert "green" in text
    assert "opt in" in text


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


def test_previously_ungated_spawners_now_gated_and_default_true(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """x-aaaf wave 2: groom/restart/evals now carry a real gate, defaulting
    True (matches their prior, ungated effective behavior)."""
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / ".fno" / "settings.yaml"))
    _write_settings(tmp_path, "schema_version: 1\n")
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    rows = collect_status(tmp_path)
    by_name = {r.name: r for r in rows}
    for name, gate_key in (
        ("groom (_spawn_groom_worker)", "config.groom.enabled"),
        ("restart (_revive_orphans)", "config.restart.enabled"),
        ("evals runner", "config.evals.enabled"),
    ):
        assert by_name[name].armed is True
        assert by_name[name].gate_key == gate_key
        assert by_name[name].rank == "config"


def test_king_loop_row_is_present_and_defaults_off(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The king loop is the first self-sustaining row in this table, and it is
    the discoverability contract for it: an autonomous spawner that does not
    appear here is exactly what this verb exists to stop hiding."""
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / ".fno" / "settings.yaml"))
    _write_settings(tmp_path, "schema_version: 1\n")
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    rows = collect_status(tmp_path)
    king = next(r for r in rows if r.name == "king loop")
    assert king.gate_key == "config.king.enabled"
    assert king.armed is False, "the king loop must default off"
    assert king.rank == "config"
    assert "board non-empty" in king.trigger


def test_master_switch_row_present_and_armed_by_default(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / ".fno" / "settings.yaml"))
    _write_settings(tmp_path, "schema_version: 1\n")
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    rows = collect_status(tmp_path)
    master = next(r for r in rows if r.name == "autonomy (master switch)")
    assert master.armed is True
    assert master.gate_key == "config.autonomy.enabled"


def test_master_switch_off_vetoes_every_other_row(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """x-aaaf wave 3: config.autonomy.enabled=false reports rank "autonomy"
    on every direct-config-read row it vetoes, not the row's own gate key."""
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / ".fno" / "settings.yaml"))
    _write_settings(tmp_path, "schema_version: 1\nautonomy:\n  enabled: false\n")
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    rows = collect_status(tmp_path)
    by_name = {r.name: r for r in rows}
    assert by_name["autonomy (master switch)"].armed is False
    for name in (
        "post-merge ritual", "pr_watch (headless PR poll)",
        "recovery sweep (crash respawn)",
        "groom (_spawn_groom_worker)", "restart (_revive_orphans)",
        "evals runner", "blueprint auto-launch",
        "keep_going (autonomous follow-up)",
    ):
        assert by_name[name].armed is False, name
        assert by_name[name].rank == "autonomy", name
    for name in (
        "advance (node-walk)", "spawn_think (context /think)",
    ):
        assert by_name[name].armed is False, name
        assert by_name[name].rank == "autonomy", name


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
