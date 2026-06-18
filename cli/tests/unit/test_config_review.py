"""Tests for config.review - the loop-check review-gate config block.

Covers the Python half of control-plane step 2 (ab-f1c5a9ed): the
`config.review.required_bots` schema. The authoritative consumer is the Rust
`fno-agents loop-check` verb (its own hand-rolled parser is tested in
crates/fno-agents); this block exists so `fno config get` and the Pydantic
schema agree on the key's shape and fail-closed semantics.

Semantics under test:
  - key absent  -> None (Rust applies the code default
    ["chatgpt-codex-connector"]; Python must not invent a default list)
  - explicit [] -> [] (the declared no-review-gate path, US3)
  - non-list    -> None + warning (fail closed to the code default, AC3-ERR)
"""
from __future__ import annotations

from pathlib import Path

import pytest


def _write_settings(tmp_path: Path, content: str) -> Path:
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


def test_review_defaults_to_none(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Absent block -> required_bots is None (code default applies Rust-side)."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.config.review.required_bots is None


def test_review_required_bots_list(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    required_bots:\n"
        "      - chatgpt-codex-connector\n      - gemini-code-assist\n",
    )
    assert settings.config.review.required_bots == [
        "chatgpt-codex-connector",
        "gemini-code-assist",
    ]


def test_review_required_bots_explicit_empty(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Explicit [] is preserved (declared no-review-gate, distinct from absent)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    required_bots: []\n",
    )
    assert settings.config.review.required_bots == []


def test_review_required_bots_scalar_fails_closed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A non-list value coerces to None (code default) instead of erroring (AC3-ERR)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    required_bots: gemini\n",
    )
    assert settings.config.review.required_bots is None


def test_review_bare_key_fails_closed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A bare `required_bots:` (YAML null) must not disable the gate."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    required_bots:\n",
    )
    assert settings.config.review.required_bots is None


# --- Cross-model review panel: agent_providers + cross_model (ab-6c8f4c61) ---


def test_review_agent_providers_defaults_empty(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Absent block -> agent_providers is an empty dict (faithful empty map)."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.config.review.agent_providers == {}


def test_review_agent_providers_mapping(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """An agent->provider mapping is read verbatim (AC2-HP)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    agent_providers:\n"
        "      ux_flow_tester: gemini\n      type_design_analyzer: gemini\n",
    )
    assert settings.config.review.agent_providers == {
        "ux_flow_tester": "gemini",
        "type_design_analyzer": "gemini",
    }


def test_review_agent_providers_scalar_fails_safe(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A non-mapping agent_providers coerces to {} (no cross-model opt-in)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    agent_providers: gemini\n",
    )
    assert settings.config.review.agent_providers == {}


def test_review_cross_model_defaults_disabled(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Absent block -> cross_model.enabled is False (existing review unchanged)."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.config.review.cross_model.enabled is False


def test_review_cross_model_enabled_true(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    cross_model:\n      enabled: true\n",
    )
    assert settings.config.review.cross_model.enabled is True


def test_review_cross_model_enabled_non_bool_fails_safe(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A non-boolean enabled coerces to False (false-enabled is the dangerous way)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    cross_model:\n      enabled: banana\n",
    )
    assert settings.config.review.cross_model.enabled is False


def test_review_cross_model_non_mapping_fails_safe(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A non-mapping `cross_model:` degrades to the default disabled block."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    cross_model: 42\n",
    )
    assert settings.config.review.cross_model.enabled is False
