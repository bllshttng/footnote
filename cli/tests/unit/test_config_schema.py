"""Tests for fno.config - Pydantic settings schema.

Task 1.1: Extend Pydantic config schema with state_dir, plans_dir,
paths block, obsidian block, validators.
"""
from __future__ import annotations

import logging
import warnings
from pathlib import Path

import pytest


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _write_settings(tmp_path: Path, content: str) -> Path:
    """Write a settings.yaml to tmp_path/.fno/ and return the path."""
    settings_dir = tmp_path / ".fno"
    settings_dir.mkdir(parents=True, exist_ok=True)
    settings_file = settings_dir / "settings.yaml"
    settings_file.write_text(content, encoding="utf-8")
    return settings_file


# ---------------------------------------------------------------------------
# AC1-ERR: Glob characters rejected
# ---------------------------------------------------------------------------


def test_glob_star_in_state_dir_rejected(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-ERR: state_dir with '*' raises ValidationError at load time."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(
        tmp_path, "schema_version: 1\nconfig:\n  state_dir: '~/.fno/*'\n"
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno.config import load_settings

    with pytest.raises(Exception, match=r"\*|\?|\[|glob"):
        load_settings()


def test_glob_question_in_paths_rejected(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-ERR: paths.graph_json with '?' raises ValidationError at load time."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(
        tmp_path,
        "schema_version: 1\nconfig:\n  paths:\n    graph_json: '~/.fno/graph?.json'\n",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno.config import load_settings

    with pytest.raises(Exception, match=r"\*|\?|\[|glob"):
        load_settings()


def test_glob_bracket_in_plans_dir_rejected(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-ERR: plans_dir with '[' raises ValidationError at load time."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(
        tmp_path, "schema_version: 1\nconfig:\n  plans_dir: '.fno/[plans]/'\n"
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno.config import load_settings

    with pytest.raises(Exception, match=r"\*|\?|\[|glob"):
        load_settings()


# ---------------------------------------------------------------------------
# AC1-FR: Process-level cache hit (same object returned on second call)
# ---------------------------------------------------------------------------


def test_load_settings_cache_hit(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-FR: load_settings returns the same object when called twice."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(tmp_path, "schema_version: 1\n")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    # Clear lru_cache so the monkeypatch env takes effect
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    from fno.config import load_settings

    first = load_settings()
    second = load_settings()
    assert first is second, "load_settings() should return the same cached object"


# ---------------------------------------------------------------------------
# AC1-FR: Unknown keys => warning, not error
# ---------------------------------------------------------------------------


def test_unknown_key_emits_warning(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    """AC1-FR: Unknown config key causes a startup WARNING when FNO_DEBUG=1, not an exception."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.setenv("FNO_DEBUG", "1")
    settings_file = _write_settings(
        tmp_path, "schema_version: 1\nconfig:\n  future_thing: true\n"
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    with caplog.at_level(logging.WARNING, logger="fno.config"):
        from fno.config import load_settings

        result = load_settings()

    assert result is not None, "load_settings() should succeed on unknown keys"
    assert any(
        "future_thing" in record.message for record in caplog.records
    ), "Expected warning mentioning the unknown key 'future_thing'"


# ---------------------------------------------------------------------------
# AC1-HP: Default values are correct
# ---------------------------------------------------------------------------


def test_default_state_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: Default state_dir is '~/.fno/'."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(tmp_path, "schema_version: 1\n")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    from fno.config import load_settings

    result = load_settings()
    assert result.state_dir == "~/.fno/"


def test_default_plans_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: Default plans_dir is '.fno/plans/'."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(tmp_path, "schema_version: 1\n")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    from fno.config import load_settings

    result = load_settings()
    assert result.plans_dir == ".fno/plans/"


def test_schema_version_defaults_to_1(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: Default schema_version is 1."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(tmp_path, "schema_version: 1\n")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    from fno.config import load_settings

    result = load_settings()
    assert result.schema_version == 1


def test_obsidian_disabled_by_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-HP: obsidian.enabled defaults to False."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(tmp_path, "schema_version: 1\n")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    from fno.config import load_settings

    result = load_settings()
    assert result.obsidian.enabled is False


# ---------------------------------------------------------------------------
# AC1-EDGE: {vault} with obsidian disabled rejected at load
# ---------------------------------------------------------------------------


def test_vault_template_with_obsidian_disabled_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE: {vault} in plans_dir with obsidian.enabled=false raises at load."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(
        tmp_path,
        "schema_version: 1\nconfig:\n  plans_dir: '{vault}/plans'\n  obsidian:\n    enabled: false\n",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    from fno.config import load_settings

    with pytest.raises(Exception, match=r"vault|obsidian"):
        load_settings()


# ---------------------------------------------------------------------------
# Fix 6: Dual ProjectBlock - top-level project is deprecated alias for config.project
# ---------------------------------------------------------------------------


def test_top_level_project_id_logs_deprecation_warning(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    """In a legacy DUAL-shape file (a `config:` block AND a top-level
    `project` block), the top-level `project.id` is the deprecated location:
    it is lifted into config.project.id and emits a deprecation WARNING.

    The model is flat now, so a bare top-level `project` with NO `config:`
    block is the canonical shape and draws no warning (see
    test_config_project_id_no_warning); only the mixed legacy file warns.
    """
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(
        tmp_path,
        "schema_version: 1\nproject:\n  id: my-top-level-project\nconfig:\n  review: {}\n",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    with caplog.at_level(logging.WARNING, logger="fno.config"):
        from fno.config import load_settings
        result = load_settings()

    assert result.project.id == "my-top-level-project"
    assert any(
        "deprecated" in record.message.lower() or "config.project" in record.message
        for record in caplog.records
    ), f"Expected deprecation warning, got: {[r.message for r in caplog.records]}"


def test_config_project_id_no_warning(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    """Fix 6: config.project.id (canonical form) emits no deprecation warning."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(
        tmp_path,
        "schema_version: 1\nconfig:\n  project:\n    id: my-project\n",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    with caplog.at_level(logging.WARNING, logger="fno.config"):
        from fno.config import load_settings
        result = load_settings()

    assert result.project.id == "my-project"
    assert not any(
        "deprecated" in record.message.lower()
        for record in caplog.records
    ), f"Unexpected deprecation warning: {[r.message for r in caplog.records]}"


# ---------------------------------------------------------------------------
# Fix 5: ObsidianBlock rejects enabled=True with no vault at construction time
# ---------------------------------------------------------------------------


def test_obsidian_enabled_true_with_no_vault_raises() -> None:
    """Fix 5: ObsidianBlock(enabled=True, vault=None) must raise ValidationError at load.

    Previously the error only surfaced at resolve time; it should fail at schema load.
    """
    from pydantic import ValidationError
    from fno.config import ObsidianBlock

    with pytest.raises(ValidationError, match=r"vault|obsidian"):
        ObsidianBlock(enabled=True, vault=None)


def test_obsidian_enabled_true_with_empty_vault_raises() -> None:
    """Fix 5: ObsidianBlock(enabled=True, vault='') must raise ValidationError."""
    from pydantic import ValidationError
    from fno.config import ObsidianBlock

    with pytest.raises(ValidationError, match=r"vault|obsidian"):
        ObsidianBlock(enabled=True, vault="")


def test_obsidian_enabled_false_with_no_vault_ok() -> None:
    """Fix 5: ObsidianBlock(enabled=False, vault=None) is fine - disabled doesn't need vault."""
    from fno.config import ObsidianBlock

    block = ObsidianBlock(enabled=False, vault=None)
    assert block.enabled is False
    assert block.vault is None


# ---------------------------------------------------------------------------
# Fix 1: {{ escape }} must not trigger vault/obsidian validation error
# ---------------------------------------------------------------------------


def test_double_brace_escape_not_rejected_as_vault(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-EDGE: plans_dir: '{{vault}}/plans' with obsidian disabled loads without error.

    {{vault}} is an escape sequence meaning literal {vault}; it must not trigger
    the 'uses {vault} but obsidian.enabled is false' error.
    """
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(
        tmp_path,
        "schema_version: 1\nconfig:\n  plans_dir: '{{vault}}/plans'\n  obsidian:\n    enabled: false\n",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    from fno.config import load_settings

    # Must NOT raise; {{vault}} is a literal escape, not a {vault} template reference
    result = load_settings()
    assert result.plans_dir == "{{vault}}/plans"


# ---------------------------------------------------------------------------
# Finding C (P1): load_settings falls through to next candidate on parse failure
# ---------------------------------------------------------------------------


def test_load_settings_falls_through_on_corrupt_project_local(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Finding C (P1): malformed project-local settings.yaml falls through to global.

    When the project-local .fno/settings.yaml is malformed (parse error),
    load_settings() must continue to try the global ~/.fno/settings.yaml
    rather than returning empty defaults.
    """
    import os

    # Create fake home with a valid global settings.yaml
    fake_home = tmp_path / "home"
    fake_home.mkdir()
    global_fno = fake_home / ".fno"
    global_fno.mkdir()
    global_settings = global_fno / "settings.yaml"
    global_settings.write_text(
        "schema_version: 1\nconfig:\n  state_dir: '/custom-from-global/'\n",
        encoding="utf-8",
    )

    # Create a project-local settings.yaml that is malformed
    repo_root = tmp_path / "repo"
    repo_root.mkdir()
    local_fno = repo_root / ".fno"
    local_fno.mkdir()
    local_settings = local_fno / "settings.yaml"
    local_settings.write_text(":::bad yaml:::\n  - broken: [unterminated", encoding="utf-8")

    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo_root))
    # Override Path.home() by patching HOME env; config loader uses Path.home()
    monkeypatch.setenv("HOME", str(fake_home))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    config_mod._loaded_from = None
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass
    if hasattr(paths_mod, "resolve_repo_root"):
        try:
            paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass

    from fno.config import load_settings

    result = load_settings()
    assert result.state_dir == "/custom-from-global/", (
        f"Expected global settings fallback (state_dir=/custom-from-global/), "
        f"got: {result.state_dir!r}"
    )


# ---------------------------------------------------------------------------
# PATH_MAX validation
# ---------------------------------------------------------------------------


def test_state_dir_exceeding_path_max_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-ERR: state_dir longer than 4096 bytes raises at load."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    long_path = "~/" + "a" * 4097
    settings_file = _write_settings(
        tmp_path, f"schema_version: 1\nconfig:\n  state_dir: '{long_path}'\n"
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    from fno.config import load_settings

    with pytest.raises(Exception, match=r"4096|PATH_MAX|too long"):
        load_settings()


# ---------------------------------------------------------------------------
# Fix 2: _load_raw warns on YAML parse failure instead of silent fallback
# ---------------------------------------------------------------------------


def test_corrupt_yaml_returns_defaults_and_logs_warning(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    """AC2-ERR: corrupt settings.yaml returns defaults AND emits a WARNING.

    A user with a broken settings file should see a warning, not silent defaults.
    """
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(tmp_path, ":::bad yaml:::\n  - broken: [unterminated")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    with caplog.at_level(logging.WARNING, logger="fno.config"):
        from fno.config import load_settings

        result = load_settings()

    # Returns defaults (not raising)
    assert result.state_dir == "~/.fno/"
    # Must have logged a warning
    assert any(
        "failed to parse" in record.message or "YAMLError" in record.message or "parse" in record.message
        for record in caplog.records
    ), f"Expected parse warning, got: {[r.message for r in caplog.records]}"


# ---------------------------------------------------------------------------
# Lookup walk: FNO_CONFIG env -> ./.fno/settings.yaml -> ~/.fno/settings.yaml -> defaults
# ---------------------------------------------------------------------------


def test_project_local_settings_anchored_to_repo_root_not_cwd(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Finding 4 (P2): project-local settings discovery uses repo root, not cwd.

    When running `fno` from a subdirectory of the repo, the loader must still
    find the repo's .fno/settings.yaml (anchored to git toplevel, not cwd).
    """
    monkeypatch.delenv("FNO_CONFIG", raising=False)

    # Set up a fake repo root with a .fno/settings.yaml
    repo_root = tmp_path / "my-repo"
    fno_dir = repo_root / ".fno"
    fno_dir.mkdir(parents=True)
    settings_file = fno_dir / "settings.yaml"
    settings_file.write_text(
        "schema_version: 1\nconfig:\n  state_dir: '/custom/from-repo-root/'\n",
        encoding="utf-8",
    )

    # Pin FNO_REPO_ROOT to the repo root (as git would report)
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo_root))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    config_mod._loaded_from = None
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(paths_mod, "resolve_repo_root"):
        paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]

    from fno.config import load_settings

    result = load_settings()
    assert result.state_dir == "/custom/from-repo-root/", (
        f"Should have loaded repo-root settings, got state_dir={result.state_dir!r}"
    )


def test_env_var_takes_precedence(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC1-FR: FNO_CONFIG env var overrides the default file locations."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    # Write a settings file with a custom state_dir at an arbitrary path
    custom_file = tmp_path / "my-settings.yaml"
    custom_file.write_text(
        "schema_version: 1\nconfig:\n  state_dir: '/custom/env/path/'\n", encoding="utf-8"
    )
    monkeypatch.setenv("FNO_CONFIG", str(custom_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    from fno.config import load_settings

    result = load_settings()
    assert result.state_dir == "/custom/env/path/"


# ---------------------------------------------------------------------------
# Fix 3: unknown-key warnings behind FNO_DEBUG, no duplicate emission
# ---------------------------------------------------------------------------


def test_unknown_key_no_warning_without_fno_debug(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    """Fix 3: unknown key emits NO warning when FNO_DEBUG is unset."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.delenv("FNO_DEBUG", raising=False)
    settings_file = _write_settings(
        tmp_path, "schema_version: 1\nconfig:\n  future_thing: true\n"
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    with caplog.at_level(logging.WARNING, logger="fno.config"):
        from fno.config import load_settings
        result = load_settings()

    assert result is not None, "load_settings() should succeed"
    future_warnings = [r for r in caplog.records if "future_thing" in r.message]
    assert len(future_warnings) == 0, (
        f"Expected NO warning for unknown key without FNO_DEBUG, got: {future_warnings}"
    )


def test_unknown_key_emits_warning_with_fno_debug(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    """Fix 3: unknown key DOES emit warning when FNO_DEBUG=1."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.setenv("FNO_DEBUG", "1")
    settings_file = _write_settings(
        tmp_path, "schema_version: 1\nconfig:\n  future_thing: true\n"
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    with caplog.at_level(logging.WARNING, logger="fno.config"):
        from fno.config import load_settings
        result = load_settings()

    assert result is not None, "load_settings() should succeed"
    future_warnings = [r for r in caplog.records if "future_thing" in r.message]
    assert len(future_warnings) >= 1, (
        f"Expected warning for unknown key with FNO_DEBUG=1, got: {[r.message for r in caplog.records]}"
    )


def test_unknown_key_not_emitted_twice(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    """Fix 3: unknown nested key must not be logged twice (no duplicate walk)."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.setenv("FNO_DEBUG", "1")
    settings_file = _write_settings(
        tmp_path, "schema_version: 1\nconfig:\n  future_thing: true\n"
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    with caplog.at_level(logging.WARNING, logger="fno.config"):
        from fno.config import load_settings
        load_settings()

    future_warnings = [r for r in caplog.records if "future_thing" in r.message]
    assert len(future_warnings) == 1, (
        f"Expected exactly 1 warning for the unknown key, got {len(future_warnings)}: "
        f"{[r.message for r in future_warnings]}"
    )


# ---------------------------------------------------------------------------
# config.blueprint.max_prs_per_epic (ab-e9c81ed3, C1)
# ---------------------------------------------------------------------------


def test_blueprint_max_prs_per_epic_default(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Default max_prs_per_epic is 4 when config.blueprint is absent."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(tmp_path, "schema_version: 1\n")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    settings = config_mod.load_settings()
    assert settings.blueprint.max_prs_per_epic == 4


def test_blueprint_max_prs_per_epic_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """config.blueprint.max_prs_per_epic is read from settings.yaml."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(
        tmp_path,
        "schema_version: 1\nconfig:\n  blueprint:\n    max_prs_per_epic: 7\n",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    settings = config_mod.load_settings()
    assert settings.blueprint.max_prs_per_epic == 7


def test_blueprint_max_prs_per_epic_rejects_non_positive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A max_prs_per_epic < 1 is rejected at load time (ceiling must be >= 1)."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(
        tmp_path,
        "schema_version: 1\nconfig:\n  blueprint:\n    max_prs_per_epic: 0\n",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]

    with pytest.raises(Exception, match=r"max_prs_per_epic|>= ?1|positive"):
        config_mod.load_settings()


