"""Unit tests for fno.setup.emit_shell codegen.

Task 2.5 of plan 2026-05-14-path-config-impl.

All tests use tmp_path + monkeypatch isolation. An autouse fixture pins
FNO_REPO_ROOT to tmp_path so resolve_repo_root() is isolated
(feedback_abi_repo_root_leaks_between_tests memory entry).
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
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass
    yield
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass


def _set_settings(monkeypatch: pytest.MonkeyPatch, tmp_path: Path, content: str) -> None:
    """Write a settings.yaml and wire it via FNO_CONFIG."""
    settings_file = tmp_path / "settings.yaml"
    settings_file.write_text(content, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))


# ---------------------------------------------------------------------------
# AC2-HP: Codegen is byte-deterministic within the same process
# ---------------------------------------------------------------------------


def test_emit_paths_sh_deterministic(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP: emit_paths_sh() is byte-identical across two calls in the same process."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.setup.emit_shell import emit_paths_sh

    first = emit_paths_sh()
    second = emit_paths_sh()
    assert first == second, "emit_paths_sh() must be byte-deterministic"
    assert len(first) > 0, "output must be non-empty"


# ---------------------------------------------------------------------------
# AC1-CRITICAL: Machine-stable output - no hardcoded absolute HOME paths
# ---------------------------------------------------------------------------


def test_emit_paths_sh_no_hardcoded_home(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-CRITICAL: emit_paths_sh uses $HOME literal, not resolved /home/user path."""
    # Simulate a different HOME to prove the output is portable
    monkeypatch.setenv("HOME", "/tmp/fake-test-home")
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")
    # Clear settings cache so the new HOME is picked up
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    # Must contain the literal string $HOME, not the resolved /tmp/fake-test-home
    assert "$HOME" in stub, f"emit_paths_sh must emit literal $HOME, got:\n{stub}"
    assert "/tmp/fake-test-home" not in stub, (
        f"emit_paths_sh must NOT embed resolved HOME path, got:\n{stub}"
    )


def test_emit_paths_sh_state_dir_uses_home_template(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-CRITICAL: STATE_DIR export uses $HOME/.fno not /users/xxx/.fno."""
    monkeypatch.setenv("HOME", "/tmp/fake-home-check")
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    assert 'export STATE_DIR="$HOME/.fno"' in stub or "STATE_DIR=$HOME" in stub, (
        f"STATE_DIR should reference $HOME, got:\n{stub}"
    )


def test_emit_paths_sh_plans_dir_uses_repo_root_template(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-CRITICAL: PLANS_DIR uses $REPO_ROOT not a hardcoded absolute path."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    assert "$REPO_ROOT" in stub or "REPO_ROOT" in stub, (
        f"PLANS_DIR should reference REPO_ROOT for project-relative paths, got:\n{stub}"
    )
    # PLANS_DIR line specifically must use $REPO_ROOT, not a hardcoded absolute path
    plans_line = next((l for l in stub.splitlines() if "PLANS_DIR=" in l), None)
    assert plans_line is not None, "PLANS_DIR export line not found in stub"
    assert str(tmp_path) not in plans_line, (
        f"PLANS_DIR must NOT embed resolved tmp_path, got line:\n{plans_line}"
    )


# ---------------------------------------------------------------------------
# AC2-HP: Generated bash is sourceable
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
# AC2-HP: Bash stub echoes GRAPH_JSON_PATH too
# ---------------------------------------------------------------------------


def test_emit_paths_sh_graph_json_exported(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP: GRAPH_JSON_PATH is exported and accessible after sourcing."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    paths_file = tmp_path / "paths.sh"
    paths_file.write_text(stub, encoding="utf-8")

    result = subprocess.run(
        ["bash", "-c", f"source {paths_file} && echo \"$GRAPH_JSON_PATH\""],
        capture_output=True,
        text=True,
        timeout=10,
    )
    assert result.returncode == 0, f"bash sourcing failed: {result.stderr}"
    assert result.stdout.strip().endswith("graph.json"), (
        f"GRAPH_JSON_PATH should end with graph.json, got: {result.stdout.strip()!r}"
    )


# ---------------------------------------------------------------------------
# AC2-EDGE: Output is well-formed even with default (no custom overrides) schema
# ---------------------------------------------------------------------------


def test_emit_paths_sh_no_custom_paths_well_formed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-EDGE: Default schema (no path overrides) produces well-formed output."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    # Must start with a bash shebang or comment
    assert stub.startswith("#!/"), f"stub must start with shebang, got: {stub[:20]!r}"
    # Must not contain Python syntax
    assert "def " not in stub, "stub must not contain Python def"
    # Must contain export statements
    assert "export STATE_DIR=" in stub, "must export STATE_DIR"
    assert "export GRAPH_JSON_PATH=" in stub, "must export GRAPH_JSON_PATH"
    # Must end with newline
    assert stub.endswith("\n"), "stub must end with newline"


# ---------------------------------------------------------------------------
# AC2-HP: paths_plan_file and paths_inbox_thread lazy functions are present
# ---------------------------------------------------------------------------


def test_emit_paths_sh_lazy_functions_present(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP: Generated stub includes paths_plan_file() and paths_inbox_thread() functions."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    assert "paths_plan_file()" in stub, "must define paths_plan_file shell function"
    assert "paths_inbox_thread()" in stub, "must define paths_inbox_thread shell function"


# ---------------------------------------------------------------------------
# AC2-HP: paths_plan_file() works correctly from bash
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
        "schema_version: 1\nconfig:\n  state_dir: '/home/*/abilities'\n",
    )
    # Clear caches after the env was set
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

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
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

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
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

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
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

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


def test_emit_paths_sh_use_defaults_machine_stable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-MACHINE-STABLE: emit_paths_sh(use_defaults=True) ignores user settings.yaml.

    Two calls with different settings.yaml files must produce byte-identical output.
    This proves that the checked-in paths.sh hash is stable across machines.
    """
    from fno.setup.emit_shell import emit_paths_sh

    # Call 1: no settings file at all
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass

    output_no_settings = emit_paths_sh(use_defaults=True)

    # Call 2: settings file with custom state_dir
    custom_settings = tmp_path / "custom_settings.yaml"
    custom_settings.write_text(
        "schema_version: 1\nconfig:\n  state_dir: '~/.custom-state'\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CONFIG", str(custom_settings))
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass

    output_custom_settings = emit_paths_sh(use_defaults=True)

    assert output_no_settings == output_custom_settings, (
        "emit_paths_sh(use_defaults=True) must be byte-identical regardless of user settings.\n"
        f"No-settings output:\n{output_no_settings}\n\n"
        f"Custom-settings output:\n{output_custom_settings}"
    )


def test_emit_paths_sh_use_defaults_config_file_uses_state_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-MACHINE-STABLE: use_defaults=True emits CONFIG_FILE=$STATE_DIR/config.toml.

    The checked-in paths.sh must not embed any machine-specific absolute path
    for CONFIG_FILE. It should use a $STATE_DIR-relative derivation instead.
    """
    # Set up custom settings so use_defaults=False would embed a real path
    custom_settings = tmp_path / "settings.yaml"
    custom_settings.write_text("schema_version: 1\n", encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(custom_settings))
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh(use_defaults=True)

    # CONFIG_FILE must NOT be the custom absolute path
    assert str(custom_settings) not in stub, (
        f"use_defaults=True must not embed user settings path {custom_settings!r}:\n{stub}"
    )
    # CONFIG_FILE must be $STATE_DIR-relative (machine-stable)
    assert "$STATE_DIR/config.toml" in stub or 'STATE_DIR/config.toml' in stub, (
        f"use_defaults=True must emit CONFIG_FILE as $STATE_DIR/config.toml:\n{stub}"
    )


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
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh(use_defaults=False)

    # The custom state_dir must appear in the output (as $HOME/.my-custom-fno)
    assert ".my-custom-fno" in stub, (
        f"use_defaults=False must reflect custom state_dir, got:\n{stub}"
    )


# ---------------------------------------------------------------------------
# HANDOFFS_DIR codegen (ab-3f6def07)
# ---------------------------------------------------------------------------


def test_emit_paths_sh_handoffs_dir_default_uses_repo_root_basename(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Default HANDOFFS_DIR resolves at source-time to $STATE_DIR/handoffs/<repo basename>."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    handoffs_line = next((l for l in stub.splitlines() if "HANDOFFS_DIR=" in l), None)
    assert handoffs_line is not None, "HANDOFFS_DIR export line not found in stub"
    assert "$STATE_DIR/handoffs/" in handoffs_line, (
        f"default HANDOFFS_DIR must derive from $STATE_DIR, got: {handoffs_line!r}"
    )
    assert "REPO_ROOT" in handoffs_line, (
        f"default HANDOFFS_DIR must include REPO_ROOT basename, got: {handoffs_line!r}"
    )


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


def test_emit_paths_sh_handoffs_dir_sourceable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Sourcing the stub from bash makes HANDOFFS_DIR resolve correctly."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    paths_file = tmp_path / "paths.sh"
    paths_file.write_text(stub, encoding="utf-8")

    # Source with a stable REPO_ROOT so the command substitution resolves
    fake_repo = tmp_path / "my-project"
    fake_repo.mkdir()
    result = subprocess.run(
        ["bash", "-c", f"REPO_ROOT='{fake_repo}' source {paths_file} && echo \"$HANDOFFS_DIR\""],
        capture_output=True,
        text=True,
        timeout=10,
    )
    assert result.returncode == 0, f"bash sourcing failed: {result.stderr}"
    out = result.stdout.strip()
    assert out.endswith("/handoffs/my-project"), (
        f"HANDOFFS_DIR should end with handoffs/<basename>, got: {out!r}"
    )
