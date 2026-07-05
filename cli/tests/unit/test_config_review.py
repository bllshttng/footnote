"""Tests for config.review - the loop-check review-gate config block.

Covers the Python half of control-plane step 2 (ab-f1c5a9ed): the
`config.review.github_apps` schema (x-4baa; `required_bots` is now a legacy
alias). The authoritative consumer is the Rust `fno-agents loop-check` verb
(its own hand-rolled parser is tested in crates/fno-agents); this block exists
so `fno config get` and the Pydantic schema agree on the key's shape and
fail-closed semantics.

Semantics under test:
  - key absent  -> None (no review gate; the Rust effective default is [],
    cv-6537099f - Python must not invent a default list)
  - explicit [] -> [] (the declared no-review-gate path, US3)
  - non-list    -> None + warning (fail closed, AC3-ERR)
  - required_bots is a legacy alias for github_apps (github_apps wins if both)
  - peers scalar-or-map coercion; a map missing `provider` or a peers block
    with no posting identity fails LOUD (fail closed, not a silent skip)
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


def test_review_required_bots_scalar_is_singleton(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A bare scalar gates on that one login (parity with the Rust reader); a
    bracket-less typo must not fail OPEN to no-gate (codex P1 on #205)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    required_bots: gemini\n",
    )
    assert settings.config.review.required_bots == ["gemini"]
    assert settings.config.review.github_apps == ["gemini"]


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


# --- github_apps rename + required_bots alias (x-4baa US1) ---


def test_github_apps_canonical_list(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """github_apps is read verbatim and mirrored onto the required_bots alias."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    github_apps:\n"
        "      - chatgpt-codex-connector\n",
    )
    assert settings.config.review.github_apps == ["chatgpt-codex-connector"]
    assert settings.config.review.required_bots == ["chatgpt-codex-connector"]


def test_required_bots_aliases_github_apps(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A legacy required_bots-only config populates github_apps identically (AC2-HP)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    required_bots:\n"
        "      - chatgpt-codex-connector\n",
    )
    assert settings.config.review.github_apps == ["chatgpt-codex-connector"]


def test_github_apps_wins_over_required_bots(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """When both are set, github_apps wins (Locked Decision 2)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n"
        "    github_apps: [new-bot]\n    required_bots: [old-bot]\n",
    )
    assert settings.config.review.github_apps == ["new-bot"]
    assert settings.config.review.required_bots == ["new-bot"]


def test_github_apps_absent_is_no_gate(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Absent -> None (no gate); the old chatgpt-codex-connector default is gone."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.config.review.github_apps is None


# --- peers / peer_identity / peer_token_env (x-4baa US2) ---


def test_peers_scalar_coerces_to_list(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n"
        "    peers: codex\n    peer_identity: fno-peer-bot\n",
    )
    assert settings.config.review.peers == ["codex"]
    assert settings.config.review.peer_identity == "fno-peer-bot"


def test_peers_list_and_identity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n"
        "    peers: [codex, gemini]\n    peer_identity: fno-peer-bot\n"
        "    peer_token_env: GH_PEER_TOKEN\n",
    )
    assert settings.config.review.peers == ["codex", "gemini"]
    assert settings.config.review.peer_token_env == "GH_PEER_TOKEN"


def test_peers_map_entry_with_own_identity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A per-peer identity map does not require the shared peer_identity."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    peers:\n"
        "      - provider: codex\n        identity: fno-codex-bot\n",
    )
    assert settings.config.review.peers == [
        {"provider": "codex", "identity": "fno-codex-bot"}
    ]


def test_peers_map_missing_provider_fails_loud(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A map entry with no `provider` is a loud config error, not a silent skip."""
    with pytest.raises(Exception, match="provider"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  review:\n    peers:\n"
            "      - identity: fno-codex-bot\n    peer_identity: x\n",
        )


def test_peers_without_identity_fails_loud(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """peers set with no posting identity can never clear -> fail closed at load."""
    with pytest.raises(Exception, match="peer_identity"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  review:\n    peers: [codex]\n",
        )


# --- optional_apps: honored-if-present, never required (x-4baa) ---


def test_optional_apps_list(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    optional_apps:\n"
        "      - chatgpt-codex-connector\n",
    )
    assert settings.config.review.optional_apps == ["chatgpt-codex-connector"]


def test_optional_apps_scalar_coerces(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    optional_apps: chatgpt-codex-connector\n",
    )
    assert settings.config.review.optional_apps == ["chatgpt-codex-connector"]


def test_optional_apps_defaults_empty(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Absent -> [] (no optional reviewers), independent of the required gate."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.config.review.optional_apps == []


# --- parser parity on non-string malformed values (codex P1 on #205) ---


def test_github_apps_numeric_scalar_gates_closed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A stray numeric scalar becomes a (never-matching) singleton, matching the
    Rust text reader - a required-gate typo fails CLOSED, not open to no-gate."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    github_apps: 123\n",
    )
    assert settings.config.review.github_apps == ["123"]


def test_github_apps_mapping_degrades_like_rust(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A mapping is not a login list -> None, agreeing with the Rust reader
    (which rejects a `{...}` scalar via scalar_as_singleton)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    github_apps: {login: codex}\n",
    )
    assert settings.config.review.github_apps is None


def test_optional_apps_scalar_and_mapping_parity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    numeric = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    optional_apps: 123\n",
    )
    assert numeric.config.review.optional_apps == ["123"]
    mapping = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    optional_apps: {a: b}\n",
    )
    assert mapping.config.review.optional_apps == []


# --- reviewers: local-attestation gate (x-e703, Phase 2) ---


def test_reviewers_defaults_empty(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Absent -> [] (no reviewers gate)."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.config.review.reviewers == []


def test_reviewers_list(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A one-entry reviewers list is exposed verbatim (AC2-HP)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    reviewers:\n      - sigma\n",
    )
    assert settings.config.review.reviewers == ["sigma"]


def test_reviewers_scalar_coerces(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    reviewers: sigma\n",
    )
    assert settings.config.review.reviewers == ["sigma"]


def test_reviewers_strips_leading_slash(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """`/code-review` and `code-review` are the same reviewer (slash stripped)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    reviewers: [/code-review, declare]\n",
    )
    assert settings.config.review.reviewers == ["code-review", "declare"]


def test_reviewers_unresolvable_fails_loud(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """An unresolvable reviewer name raises loudly naming it (AC2-ERR / AC3-ERR):
    a typo must never silently drop to a never-green gate."""
    with pytest.raises(Exception) as excinfo:
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  review:\n    reviewers: [teleport]\n",
        )
    assert "teleport" in str(excinfo.value)
