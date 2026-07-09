"""Tests for config.post_merge - the /fno:pr merged skill config block.

Covers the mechanical, deterministic half of the post-merge ritual BDD:
per-project parking-lot-path resolution and the "missing config fails loud /
never guesses" guarantee. The skill reads these values via `fno config get
config.post_merge.parking_lot_path`, so the CliRunner tests below exercise the
exact resolution path the skill uses (in-process, against the source schema).
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner


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


def _load(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, content: str):
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(tmp_path, content)
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    return config_mod.load_settings()


def _config_get(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, key: str, content: str):
    """Invoke `fno config get <key>` in-process against the source schema."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(tmp_path, content)
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    from fno.cli import app

    return CliRunner().invoke(app, ["config", "get", key])


# ---------------------------------------------------------------------------
# Schema defaults + overrides
# ---------------------------------------------------------------------------


def test_post_merge_defaults(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """With no config.post_merge block: parking_lot_path unset, enabled True.

    parking_lot_path must default to None (not a guessed path) so the skill
    fails loud rather than writing to the wrong queue.
    """
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.post_merge.parking_lot_path is None
    assert settings.post_merge.enabled is True


def test_post_merge_parking_lot_path_set(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """parking_lot_path is read verbatim from settings (vault-area != project name)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  post_merge:\n"
        "    parking_lot_path: internal/etl/backlog/parking-lot.md\n",
    )
    assert (
        settings.post_merge.parking_lot_path
        == "internal/etl/backlog/parking-lot.md"
    )
    assert settings.post_merge.enabled is True


def test_post_merge_enabled_false(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """enabled: false is honored so a repo can opt out without removing the path."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  post_merge:\n"
        "    parking_lot_path: internal/web/backlog/parking-lot.md\n"
        "    enabled: false\n",
    )
    assert (
        settings.post_merge.parking_lot_path
        == "internal/web/backlog/parking-lot.md"
    )
    assert settings.post_merge.enabled is False


def test_post_merge_parking_lot_path_rejects_glob(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A glob char in parking_lot_path is rejected at load (mirrors path-field policy)."""
    with pytest.raises(Exception, match=r"\*|\?|\[|glob"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  post_merge:\n"
            "    parking_lot_path: 'internal/*/backlog/parking-lot.md'\n",
        )


def test_post_merge_parking_lot_path_rejects_absolute(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """An absolute parking_lot_path is rejected: it must be repo-relative."""
    with pytest.raises(Exception, match=r"repo-relative|/|~"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  post_merge:\n"
            "    parking_lot_path: /etc/evil.md\n",
        )


def test_post_merge_parking_lot_path_rejects_tilde(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A '~'-anchored parking_lot_path is rejected: it must be repo-relative."""
    with pytest.raises(Exception, match=r"repo-relative|/|~"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  post_merge:\n"
            "    parking_lot_path: '~/evil.md'\n",
        )


def test_post_merge_parking_lot_path_rejects_dotdot(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A '..' segment is rejected so the skill cannot write outside the repo."""
    with pytest.raises(Exception, match=r"\.\.|escape"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  post_merge:\n"
            "    parking_lot_path: ../../outside/parking-lot.md\n",
        )


# ---------------------------------------------------------------------------
# CLI resolution path (`fno config get`) - exactly what the skill calls
# ---------------------------------------------------------------------------


def test_config_get_resolves_parking_lot_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC6-HP: `config get` returns the configured parking-lot path."""
    result = _config_get(
        tmp_path,
        monkeypatch,
        "config.post_merge.parking_lot_path",
        "schema_version: 1\nconfig:\n  post_merge:\n"
        "    parking_lot_path: internal/etl/backlog/parking-lot.md\n",
    )
    assert result.exit_code == 0, result.output
    assert result.output.strip() == "internal/etl/backlog/parking-lot.md"


def test_config_get_unset_parking_lot_path_is_empty(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC6-ERR: Missing config: `config get` prints an empty value (exit 0).

    The known key resolves to None, which prints as an empty line. The skill
    treats empty output as 'unset' and fails loud - it never guesses a path.
    """
    result = _config_get(
        tmp_path, monkeypatch, "config.post_merge.parking_lot_path", "schema_version: 1\n"
    )
    assert result.exit_code == 0, result.output
    assert result.output.strip() == ""


def test_config_get_enabled_flag(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """`config get config.post_merge.enabled` reflects the opt-out toggle."""
    result = _config_get(
        tmp_path,
        monkeypatch,
        "config.post_merge.enabled",
        "schema_version: 1\nconfig:\n  post_merge:\n"
        "    parking_lot_path: internal/fno/backlog/parking-lot.md\n"
        "    enabled: false\n",
    )
    assert result.exit_code == 0, result.output
    assert result.output.strip() == "False"


# ---------------------------------------------------------------------------
# self_reap - opt-in agent-view row self-removal (Step 8)
# ---------------------------------------------------------------------------


def test_post_merge_self_reap_defaults_false(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No config: self_reap is False so finished workers print the reap command,
    never auto-remove their row (the safe default after the over-reap incident)."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.post_merge.self_reap is False


def test_post_merge_self_reap_true(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """An explicit affirmative opts a repo into auto-reaping finished merged workers."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  post_merge:\n"
        "    parking_lot_path: internal/fno/backlog/parking-lot.md\n"
        "    self_reap: true\n",
    )
    assert settings.post_merge.self_reap is True


def test_post_merge_self_reap_typo_coerces_false(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A malformed value fails safe to False (auto-removing a wanted row is the
    costly direction), and never breaks load_settings() for other consumers."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  post_merge:\n    self_reap: banana\n",
    )
    assert settings.post_merge.self_reap is False


def test_config_get_self_reap_flag(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """`config get config.post_merge.self_reap` - exactly what Step 8 reads."""
    result = _config_get(
        tmp_path,
        monkeypatch,
        "config.post_merge.self_reap",
        "schema_version: 1\nconfig:\n  post_merge:\n    self_reap: true\n",
    )
    assert result.exit_code == 0, result.output
    assert result.output.strip() == "True"


# ---------------------------------------------------------------------------
# model - the post-merge worker tier knob
# ---------------------------------------------------------------------------


def test_post_merge_model_defaults_sonnet(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No config: model defaults to sonnet (the Step-6 diff-judgment needs real
    reasoning), never the account default (Fable)."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.post_merge.model == "claude-sonnet-5"


def test_post_merge_model_override(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """An explicit model is read verbatim."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  post_merge:\n    model: claude-opus-4-8\n",
    )
    assert settings.post_merge.model == "claude-opus-4-8"


def test_post_merge_model_empty_coerces_default(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A blank model must never reach `--model ""` - it coerces to the default."""
    settings = _load(
        tmp_path,
        monkeypatch,
        'schema_version: 1\nconfig:\n  post_merge:\n    model: ""\n',
    )
    assert settings.post_merge.model == "claude-sonnet-5"
