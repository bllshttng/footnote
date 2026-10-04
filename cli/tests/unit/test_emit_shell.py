"""Unit tests for fno.setup.emit_shell codegen.

Task 2.5 of plan 2026-05-14-path-config-impl.

All tests use tmp_path + monkeypatch isolation. An autouse fixture pins
FNO_REPO_ROOT to tmp_path so resolve_repo_root() is isolated
(feedback_fno_repo_root_leaks_between_tests memory entry).
"""
from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Generator

import pytest


# ---------------------------------------------------------------------------
# Autouse fixture: pin FNO_REPO_ROOT and clear caches before each test
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    """Isolate each test: reset caches and pin repo root + settings."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    import fno.paths as paths_mod
    yield
def _set_settings(monkeypatch: pytest.MonkeyPatch, tmp_path: Path, content: str) -> None:
    """Write a settings.yaml and wire it via FNO_CONFIG."""
    settings_file = tmp_path / "settings.yaml"
    settings_file.write_text(content, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))


# ---------------------------------------------------------------------------
# AC2-HP: Codegen is byte-deterministic within the same process
# ---------------------------------------------------------------------------


def test_emit_paths_sh_sourceable_bash(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP: Generated paths.sh is sourceable by bash and echoes STATE_DIR."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    paths_file = tmp_path / "paths.sh"
    paths_file.write_text(stub, encoding="utf-8")

    result = subprocess.run(
        ["bash", "-c", f"source {paths_file} && echo \"$STATE_DIR\""],
        capture_output=True,
        text=True,
        timeout=10,
    )
    assert result.returncode == 0, f"bash sourcing failed: {result.stderr}"
    state_dir_output = result.stdout.strip()
    assert state_dir_output, "STATE_DIR must be non-empty after sourcing"
    assert "/" in state_dir_output, f"STATE_DIR should be an absolute path, got: {state_dir_output!r}"


# ---------------------------------------------------------------------------
# AC2-EDGE: Output is well-formed even with default (no custom overrides) schema
# ---------------------------------------------------------------------------


def test_emit_paths_sh_plan_file_function_works(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP: paths_plan_file() returns PLANS_DIR/name when called from bash."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    paths_file = tmp_path / "paths.sh"
    paths_file.write_text(stub, encoding="utf-8")

    result = subprocess.run(
        ["bash", "-c", f"source {paths_file} && paths_plan_file my-plan.md"],
        capture_output=True,
        text=True,
        timeout=10,
    )
    assert result.returncode == 0, f"paths_plan_file failed: {result.stderr}"
    output = result.stdout.strip()
    assert output.endswith("my-plan.md"), f"got: {output!r}"


# ---------------------------------------------------------------------------
# AC2-FR: Pydantic validation failure surfaces a clear error
# ---------------------------------------------------------------------------


def test_emit_paths_sh_validation_failure_clear_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-FR: A settings.yaml with glob chars in state_dir raises a clear error."""
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\nconfig:\n  state_dir: '/home/*/fno'\n",
    )
    # Clear caches after the env was set
    from fno import config as config_mod
    import fno.paths as paths_mod

    from fno.setup.emit_shell import emit_paths_sh

    with pytest.raises(Exception) as exc_info:
        emit_paths_sh()
    # The error should mention glob, validation, or the specific char
    msg = str(exc_info.value).lower()
    assert any(word in msg for word in ("glob", "validat", "*", "invalid")), (
        f"Expected validation error about glob chars, got: {exc_info.value}"
    )


def test_is_project_relative_rejects_template_anywhere(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HIGH (Gemini): _is_project_relative returns False when '{' appears anywhere.

    A value like 'plans/{project}' contains a template var not at start;
    emitting it as '$REPO_ROOT/plans/{project}' produces unexpandable bash.
    The check must disqualify ANY occurrence of '{', not just at the start.
    """
    from fno.setup.emit_shell import _is_project_relative

    # Bare relative (no template vars) - should be True
    assert _is_project_relative(".fno/plans")
    assert _is_project_relative("plans")

    # Template var anywhere - must return False so emit falls through
    assert not _is_project_relative("plans/{project}")
    assert not _is_project_relative("{project}/plans")
    assert not _is_project_relative("some/path/{vault}/plans")

    # Absolute / home-relative / env-var - already false
    assert not _is_project_relative("/abs/path")
    assert not _is_project_relative("~/plans")
    assert not _is_project_relative("$SOME_VAR/plans")


# ---------------------------------------------------------------------------
# Finding B (P1): Template values ({vault}, {project}) resolved at codegen time
# ---------------------------------------------------------------------------


def test_emit_paths_sh_vault_template_resolved_at_codegen(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Finding B (P1): state_dir with {vault} template is resolved to absolute at codegen time.

    Shell consumers can't expand {vault} or {project}; emit_paths_sh must resolve
    them via paths.state_dir() at codegen time and emit the absolute value.
    """
    vault_path = tmp_path / "vault"
    vault_path.mkdir()
    settings_content = (
        "schema_version: 1\n"
        "config:\n"
        f"  state_dir: '{vault_path}/state'\n"
        "  obsidian:\n"
        "    enabled: true\n"
        f"    vault: '{vault_path}'\n"
    )
    settings_file = tmp_path / "settings.yaml"
    settings_file.write_text(settings_content, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))
    from fno import config as config_mod
    import fno.paths as paths_mod

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    expected = str(vault_path / "state")
    assert expected in stub, (
        f"STATE_DIR must contain resolved vault path {expected!r}, got:\n{stub}"
    )
    assert "{vault}" not in stub, (
        f"Stub must not contain raw {{vault}} template, got:\n{stub}"
    )


def test_emit_paths_sh_vault_template_in_state_dir_no_raw_brace(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Finding B (P1): Raw {vault} tokens must NOT appear in the emitted shell stub.

    If the config uses {vault}/state as state_dir, the emit function must
    resolve it at codegen time; shell can't expand Python-style templates.
    """
    vault_path = tmp_path / "obsidian-vault"
    vault_path.mkdir()
    settings_content = (
        "schema_version: 1\n"
        "config:\n"
        "  state_dir: '{vault}/state'\n"
        "  obsidian:\n"
        "    enabled: true\n"
        f"    vault: '{vault_path}'\n"
    )
    settings_file = tmp_path / "settings.yaml"
    settings_file.write_text(settings_content, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))
    from fno import config as config_mod
    import fno.paths as paths_mod

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    assert "{vault}" not in stub, (
        f"Emitted stub must not contain unexpanded {{vault}} template:\n{stub}"
    )
    assert "{project}" not in stub, (
        f"Emitted stub must not contain unexpanded {{project}} template:\n{stub}"
    )


# ---------------------------------------------------------------------------
# Finding A (P1): CONFIG_FILE export uses actual loaded path, not $STATE_DIR
# ---------------------------------------------------------------------------


def test_emit_paths_sh_config_file_uses_actual_loaded_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Finding A (P1): CONFIG_FILE in emitted shell reflects the path load_settings() used.

    When settings are loaded from a project-local .fno/settings.yaml,
    CONFIG_FILE should be that project-local path, not '$STATE_DIR/settings.yaml'.
    """
    # Write a project-local settings.yaml
    project_local = tmp_path / ".fno" / "settings.yaml"
    project_local.parent.mkdir(parents=True)
    project_local.write_text("schema_version: 1\n", encoding="utf-8")
    # Wire FNO_CONFIG so the loader picks up the project-local file
    monkeypatch.setenv("FNO_CONFIG", str(project_local))
    # Clear caches so the fresh env is picked up
    from fno import config as config_mod
    import fno.paths as paths_mod

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    # CONFIG_FILE must be the absolute path of the project-local settings file
    expected_path = str(project_local.resolve())
    assert f'export CONFIG_FILE=' in stub, f"CONFIG_FILE not exported in stub:\n{stub}"
    assert expected_path in stub, (
        f"CONFIG_FILE must contain actual loaded path {expected_path!r}, "
        f"but got stub without it:\n{stub}"
    )
    # Must NOT be the generic $STATE_DIR/settings.yaml derivation
    assert "CONFIG_FILE=$STATE_DIR" not in stub and 'CONFIG_FILE="$STATE_DIR' not in stub, (
        f"CONFIG_FILE must not be $STATE_DIR/settings.yaml, got:\n{stub}"
    )


# ---------------------------------------------------------------------------
# AC1-MACHINE-STABLE: use_defaults=True produces identical output on any machine
# ---------------------------------------------------------------------------


def test_emit_paths_sh_use_defaults_false_reflects_user_settings(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-LIVE: emit_paths_sh(use_defaults=False) reflects user settings.

    When the user has a custom state_dir, use_defaults=False should embed it.
    """
    custom_settings = tmp_path / "settings.yaml"
    custom_settings.write_text(
        "schema_version: 1\nconfig:\n  state_dir: '~/.my-custom-fno'\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CONFIG", str(custom_settings))
    from fno import config as config_mod
    import fno.paths as paths_mod
    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh(use_defaults=False)

    # The custom state_dir must appear in the output (as $HOME/.my-custom-fno)
    assert ".my-custom-fno" in stub, (
        f"use_defaults=False must reflect custom state_dir, got:\n{stub}"
    )


# ---------------------------------------------------------------------------
# HANDOFFS_DIR codegen (ab-3f6def07)
# ---------------------------------------------------------------------------



def test_emit_paths_sh_handoffs_dir_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """HANDOFFS_DIR honors config.paths.handoffs_dir override."""
    custom = tmp_path / "shared-handoffs"
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\n"
        "config:\n"
        f"  paths:\n    handoffs_dir: '{custom}'\n",
    )

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    assert str(custom) in stub, (
        f"explicit handoffs_dir override must appear in stub, got:\n{stub}"
    )


def test_emit_paths_sh_handoffs_dir_uses_project_id_when_set(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """When config.project.id is set, HANDOFFS_DIR uses it as a static path
    matching paths.handoffs_dir() (Python/shell parity, gemini PR #298 review)."""
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\n"
        "config:\n"
        "  project:\n    id: 'my-pinned-id'\n",
    )

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    handoffs_line = next((l for l in stub.splitlines() if "HANDOFFS_DIR=" in l), None)
    assert handoffs_line is not None, "HANDOFFS_DIR export line not found"
    assert "my-pinned-id" in handoffs_line, (
        f"HANDOFFS_DIR must embed project.id when set, got: {handoffs_line!r}"
    )
    assert "basename" not in handoffs_line, (
        f"HANDOFFS_DIR must NOT fall back to basename when project.id is set, got: {handoffs_line!r}"
    )


def test_emit_paths_sh_defaults_match_checked_in_fixture() -> None:
    """The defaults emitter must reproduce the checked-in paths.sh byte for byte.

    The Rust verb generates the file now, and verify's schema hash has to agree
    with Rust output until the verify verb ports.
    """
    import fno
    from fno.setup.emit_shell import emit_paths_sh

    repo_root = Path(fno.__file__).resolve().parents[3]
    fixture = (repo_root / "scripts" / "lib" / "paths.sh").read_text(encoding="utf-8")
    assert emit_paths_sh(use_defaults=True) == fixture


