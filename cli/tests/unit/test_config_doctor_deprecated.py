"""Tests for `fno config doctor`'s deprecated-key advisory (x-4be1).

`dispatch.auto_merge` reads as an `auto_merge.grant` value for one release.
The doctor advisory names WHICH file still carries the legacy spelling and the
exact migration command, so the operator can move it without grepping.
"""
from __future__ import annotations

from pathlib import Path

from typer.testing import CliRunner

from fno.config_cli import _report_deprecated_auto_merge

runner = CliRunner()


def _pin_global(monkeypatch, tmp_path: Path, body: str) -> Path:
    glob = tmp_path / "global-config.toml"
    glob.write_text(body, encoding="utf-8")
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(glob))
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    return glob


def test_legacy_true_names_file_value_and_migration(monkeypatch, tmp_path):
    glob = _pin_global(monkeypatch, tmp_path, "[dispatch]\nauto_merge = true\n")
    out: list[str] = []
    import typer

    monkeypatch.setattr(typer, "echo", lambda m, **k: out.append(m))
    _report_deprecated_auto_merge()
    text = "\n".join(out)
    assert str(glob) in text
    assert "dispatch.auto_merge" in text
    assert '"dispatch"' in text
    assert "fno config set auto_merge.grant dispatch" in text


def test_legacy_false_reads_as_none(monkeypatch, tmp_path):
    _pin_global(monkeypatch, tmp_path, "[dispatch]\nauto_merge = false\n")
    out: list[str] = []
    import typer

    monkeypatch.setattr(typer, "echo", lambda m, **k: out.append(m))
    _report_deprecated_auto_merge()
    text = "\n".join(out)
    assert '"none"' in text
    assert "fno config set auto_merge.grant none" in text


def test_no_legacy_key_is_silent(monkeypatch, tmp_path):
    _pin_global(monkeypatch, tmp_path, "[auto_merge]\ngrant = \"dispatch\"\n")
    out: list[str] = []
    import typer

    monkeypatch.setattr(typer, "echo", lambda m, **k: out.append(m))
    _report_deprecated_auto_merge()
    assert out == []


def test_wrapped_shape_also_reported(monkeypatch, tmp_path):
    """A pre-migration config-wrapped file carries the key under
    `config.dispatch`; the advisory must still find it."""
    glob = _pin_global(monkeypatch, tmp_path, "[config.dispatch]\nauto_merge = true\n")
    out: list[str] = []
    import typer

    monkeypatch.setattr(typer, "echo", lambda m, **k: out.append(m))
    _report_deprecated_auto_merge()
    text = "\n".join(out)
    assert str(glob) in text
    assert '"dispatch"' in text


def test_migration_removes_legacy_key_not_just_sets_canonical(monkeypatch, tmp_path):
    """The command must drop dispatch.auto_merge too, or the warning recurs
    forever; and it must carry the right scope flag for the file it names."""
    _pin_global(monkeypatch, tmp_path, "[dispatch]\nauto_merge = true\n")
    out: list[str] = []
    import typer

    monkeypatch.setattr(typer, "echo", lambda m, **k: out.append(m))
    _report_deprecated_auto_merge()
    text = "\n".join(out)
    assert "fno config unset dispatch.auto_merge" in text


def test_project_file_migration_carries_local_flag(monkeypatch, tmp_path):
    """A project-local legacy key needs --local: `fno config set` defaults to
    the global file, so a bare command would edit the wrong file."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    proj = tmp_path / "proj" / ".fno"
    proj.mkdir(parents=True)
    (proj / "config.toml").write_text("[dispatch]\nauto_merge = true\n", encoding="utf-8")
    glob = tmp_path / "elsewhere-config.toml"
    glob.write_text("schema_version = 1\n", encoding="utf-8")

    import fno.paths as paths_mod
    from fno import config as config_mod

    monkeypatch.setattr(paths_mod, "resolve_repo_root", lambda: tmp_path / "proj")
    monkeypatch.setattr(paths_mod, "resolve_canonical_repo_root", lambda: tmp_path / "proj")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(glob))
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    out: list[str] = []
    import typer

    monkeypatch.setattr(typer, "echo", lambda m, **k: out.append(m))
    _report_deprecated_auto_merge()
    text = "\n".join(out)
    assert str(proj / "config.toml") in text
    assert "--local" in text
    assert "fno config unset dispatch.auto_merge --local" in text
