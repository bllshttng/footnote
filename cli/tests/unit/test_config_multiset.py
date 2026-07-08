"""Tests for multi-key `fno config set a.b=1 c.d=2`.

Covers AC2-HP (both set, one lock), AC2-ERR (one invalid -> writes nothing),
AC2-UI (each pair + scope/path once), AC2-EDGE (same key twice -> last wins),
AC2-FR (mid-batch write failure leaves the pre-batch file).
"""
from __future__ import annotations

import pytest
import tomllib
from typer.testing import CliRunner

from fno.config.writer import ConfigSetError, set_config_values


def _read(tmp_path):
    return tomllib.loads((tmp_path / ".fno" / "config.toml").read_text())


def test_ac2_hp_multi_set_both_applied(tmp_path):
    results = set_config_values(
        [
            ("config.agents.a2a.auto", "false"),
            ("config.agents.a2a.turn_ceiling", "7"),
        ],
        scope="project",
        repo_root=tmp_path,
    )
    assert len(results) == 2
    data = _read(tmp_path)
    assert data["agents"]["a2a"]["auto"] is False
    assert data["agents"]["a2a"]["turn_ceiling"] == 7


def test_multi_set_across_blocks(tmp_path):
    set_config_values(
        [
            ("config.agents.a2a.auto", "true"),
            ("config.auto_merge.enabled", "true"),
        ],
        scope="project",
        repo_root=tmp_path,
    )
    data = _read(tmp_path)
    assert data["agents"]["a2a"]["auto"] is True
    assert data["auto_merge"]["enabled"] is True


def test_ac2_err_one_invalid_writes_nothing(tmp_path):
    # Seed a valid file first.
    set_config_values(
        [("config.agents.a2a.auto", "false")], scope="project", repo_root=tmp_path
    )
    before = (tmp_path / ".fno" / "config.toml").read_text()
    with pytest.raises(ConfigSetError) as exc:
        set_config_values(
            [
                ("config.agents.a2a.turn_ceiling", "9"),  # valid
                ("config.agents.a2a.turn_ceiling", "0"),  # invalid (>=1)
            ],
            scope="project",
            repo_root=tmp_path,
        )
    # The last-wins value (0) is the one validated and it fails.
    assert exc.value.exit_code == 2
    # Nothing from the batch was written.
    assert (tmp_path / ".fno" / "config.toml").read_text() == before


def test_ac2_err_unknown_key_in_batch_exit1(tmp_path):
    with pytest.raises(ConfigSetError) as exc:
        set_config_values(
            [
                ("config.agents.a2a.auto", "false"),
                ("config.bogus.key", "x"),
            ],
            scope="project",
            repo_root=tmp_path,
        )
    assert exc.value.exit_code == 1
    assert not (tmp_path / ".fno" / "config.toml").exists()


def test_ac2_edge_same_key_twice_last_wins(tmp_path):
    results = set_config_values(
        [
            ("config.agents.a2a.turn_ceiling", "3"),
            ("config.agents.a2a.turn_ceiling", "8"),
        ],
        scope="project",
        repo_root=tmp_path,
    )
    assert _read(tmp_path)["agents"]["a2a"]["turn_ceiling"] == 8
    # Deduped to a single result for the key.
    assert len(results) == 1
    assert results[0].value == 8


def test_multi_set_cross_field_block_validates_on_final_state(tmp_path):
    """A batch setting two cross-field-coupled keys in the same block must
    validate the FINAL state, not the intermediate one. config.obsidian.enabled
    requires .vault; enabled-first must NOT abort.
    """
    # enabled listed first (the order that previously aborted on enabled=true).
    results = set_config_values(
        [
            ("config.obsidian.enabled", "true"),
            ("config.obsidian.vault", "MyVault"),
        ],
        scope="project",
        repo_root=tmp_path,
    )
    assert len(results) == 2
    data = _read(tmp_path)
    assert data["obsidian"]["enabled"] is True
    assert data["obsidian"]["vault"] == "MyVault"


def test_multi_set_cross_field_still_rejects_truly_invalid(tmp_path):
    # enabled=true with NO vault in the batch is genuinely invalid -> reject,
    # write nothing.
    with pytest.raises(ConfigSetError) as exc:
        set_config_values(
            [("config.obsidian.enabled", "true")],
            scope="project",
            repo_root=tmp_path,
        )
    assert exc.value.exit_code == 2
    assert not (tmp_path / ".fno" / "config.toml").exists()


def test_empty_batch_rejected(tmp_path):
    with pytest.raises(ConfigSetError) as exc:
        set_config_values([], scope="project", repo_root=tmp_path)
    assert exc.value.exit_code == 2


def test_ac2_fr_midbatch_write_failure_intact(tmp_path, monkeypatch):
    set_config_values(
        [("config.agents.a2a.auto", "true")], scope="project", repo_root=tmp_path
    )
    before = (tmp_path / ".fno" / "config.toml").read_text()

    import fno.config.writer as writer_mod

    def _boom(*a, **k):
        raise OSError("disk full")

    monkeypatch.setattr(writer_mod.tomli_w, "dumps", _boom)
    with pytest.raises(ConfigSetError):
        set_config_values(
            [
                ("config.agents.a2a.auto", "false"),
                ("config.auto_merge.enabled", "true"),
            ],
            scope="project",
            repo_root=tmp_path,
        )
    assert (tmp_path / ".fno" / "config.toml").read_text() == before


# --- CLI surface ---


def test_ac2_ui_cli_multi_lists_each_and_scope_once(tmp_path, monkeypatch):
    gpath = tmp_path / "g.yaml"
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(gpath))
    from fno.config_cli import app

    res = CliRunner().invoke(
        app,
        ["set", "config.agents.a2a.auto=false", "config.auto_merge.enabled=true"],
    )
    assert res.exit_code == 0, res.output
    assert "config.agents.a2a.auto" in res.output
    assert "config.auto_merge.enabled" in res.output
    # The scope appears exactly once (a single trailing summary line).
    assert res.output.count("global") == 1
    data = tomllib.loads((gpath.parent / "config.toml").read_text())
    assert data["agents"]["a2a"]["auto"] is False
    assert data["auto_merge"]["enabled"] is True


def test_cli_legacy_two_token_form_still_works(tmp_path, monkeypatch):
    gpath = tmp_path / "g.yaml"
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(gpath))
    from fno.config_cli import app

    res = CliRunner().invoke(app, ["set", "config.agents.a2a.auto", "false"])
    assert res.exit_code == 0, res.output
    assert tomllib.loads((gpath.parent / "config.toml").read_text())["agents"]["a2a"]["auto"] is False


def test_cli_single_keyeq_value_token(tmp_path, monkeypatch):
    gpath = tmp_path / "g.yaml"
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(gpath))
    from fno.config_cli import app

    res = CliRunner().invoke(app, ["set", "config.agents.a2a.turn_ceiling=5"])
    assert res.exit_code == 0, res.output
    assert tomllib.loads((gpath.parent / "config.toml").read_text())["agents"]["a2a"]["turn_ceiling"] == 5


def test_cli_bare_token_without_eq_errors(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "g.yaml"))
    from fno.config_cli import app

    # One token, no '=' and not the 2-token legacy form -> usage error.
    res = CliRunner().invoke(app, ["set", "config.agents.a2a.auto"])
    assert res.exit_code != 0
