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
  - peers scalar-or-map coercion; a map missing `provider` fails LOUD
  - identity-free peers select the head-pinned local-attestation gate
  - explicit peer identities preserve the legacy posted-GitHub-review gate
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
    assert settings.review.required_bots is None


def test_review_required_bots_list(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    required_bots:\n"
        "      - chatgpt-codex-connector\n      - gemini-code-assist\n",
    )
    assert settings.review.required_bots == [
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
    assert settings.review.required_bots == []


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
    assert settings.review.required_bots == ["gemini"]
    assert settings.review.github_apps == ["gemini"]


def test_review_bare_key_fails_closed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A bare `required_bots:` (YAML null) must not disable the gate."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    required_bots:\n",
    )
    assert settings.review.required_bots is None


# --- Cross-model review panel: agent_providers + cross_model (ab-6c8f4c61) ---


def test_review_agent_providers_defaults_empty(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Absent block -> agent_providers is an empty dict (faithful empty map)."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.review.agent_providers == {}


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
    assert settings.review.agent_providers == {
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
    assert settings.review.agent_providers == {}


def test_review_agent_routes_empty_stays_legal(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The key retired with the sigma panel it routed: absent or empty loads
    clean, so a config that never used it is untouched."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    agent_routes: {}\n",
    )
    assert settings.review.agent_routes == {}


def test_review_agent_routes_configured_refuses_with_replacement(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC8-ERR: a stale sigma routing config fails loud and names the rung
    vocabulary as the replacement, never silently routing nothing."""
    with pytest.raises(ValueError, match="review-posture"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  review:\n    agent_routes:\n"
            "      code_reviewer:\n        harness: claude\n        provider: zai\n"
            "        model: glm-5.2\n",
        )


def test_review_cross_model_defaults_disabled(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Absent block -> cross_model.enabled is False (existing review unchanged)."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.review.cross_model.enabled is False


def test_review_cross_model_enabled_true(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    cross_model:\n      enabled: true\n",
    )
    assert settings.review.cross_model.enabled is True


def test_review_cross_model_enabled_non_bool_fails_safe(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A non-boolean enabled coerces to False (false-enabled is the dangerous way)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    cross_model:\n      enabled: banana\n",
    )
    assert settings.review.cross_model.enabled is False


def test_review_cross_model_non_mapping_fails_safe(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A non-mapping `cross_model:` degrades to the default disabled block."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    cross_model: 42\n",
    )
    assert settings.review.cross_model.enabled is False


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
    assert settings.review.github_apps == ["chatgpt-codex-connector"]
    assert settings.review.required_bots == ["chatgpt-codex-connector"]


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
    assert settings.review.github_apps == ["chatgpt-codex-connector"]


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
    assert settings.review.github_apps == ["new-bot"]
    assert settings.review.required_bots == ["new-bot"]


def test_github_apps_absent_is_no_gate(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Absent -> None (no gate); the old chatgpt-codex-connector default is gone."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.review.github_apps is None


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
    assert settings.review.peers == ["codex"]
    assert settings.review.peer_identity == "fno-peer-bot"


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
    assert settings.review.peers == ["codex", "gemini"]
    assert settings.review.peer_token_env == "GH_PEER_TOKEN"


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
    assert settings.review.peers == [
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


def test_peers_without_identity_selects_local_attestation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A peer is another harness; no second GitHub identity is required."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    peers: [codex]\n",
    )
    assert settings.review.peers == ["codex"]
    assert settings.review.peer_identity is None


def test_routed_claude_peer_without_identity_loads_for_local_attestation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    peers:\n"
        '      - provider: claude\n        model: "zai,glm-5.2"\n',
    )
    assert settings.review.peers == [
        {"provider": "claude", "model": "zai,glm-5.2"}
    ]


# --- claude->routed-model peer lane (x-ef41) ---


def test_peers_claude_with_model_route_loads(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC-HP: a claude peer that names a model route loads (GLM via claude CLI)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    peers:\n"
        '      - provider: claude\n        model: "zai,glm-5.2"\n'
        "    peer_identity: fno-peer-bot\n",
    )
    assert settings.review.peers == [
        {"provider": "claude", "model": "zai,glm-5.2"}
    ]


def test_peers_bare_claude_no_model_fails_loud(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC-ERR: a claude peer with no model route is the author's own model ->
    reject at load, citing the distinct-model trust invariant."""
    with pytest.raises(Exception, match="distinct-model"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  review:\n    peers:\n"
            "      - provider: claude\n    peer_identity: fno-peer-bot\n",
        )


def test_peers_scalar_claude_fails_loud(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A scalar `peers: claude` is also a routeless claude peer -> rejected."""
    with pytest.raises(Exception, match="distinct-model"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  review:\n    peers: [claude]\n"
            "    peer_identity: fno-peer-bot\n",
        )


def test_peers_claude_malformed_route_fails_loud(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A claude peer whose model is not `route_provider,route_model` is rejected
    (a bare `zai` with no model half is not a valid distinct route)."""
    with pytest.raises(Exception, match="distinct-model"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  review:\n    peers:\n"
            '      - provider: claude\n        model: "zai"\n'
            "    peer_identity: fno-peer-bot\n",
        )


@pytest.mark.parametrize("route", ["anthropic,claude-opus", "claude,sonnet"])
def test_peers_claude_same_model_route_fails_loud(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, route: str
) -> None:
    """A claude peer routing back to the author's own provider (anthropic/claude)
    is not a distinct model -> rejected at load (defense-in-depth: the runtime
    also fail-safes an unknown provider to None)."""
    with pytest.raises(Exception, match="distinct-model"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  review:\n    peers:\n"
            f'      - provider: claude\n        model: "{route}"\n'
            "    peer_identity: fno-peer-bot\n",
        )


def test_peers_codex_with_model_key_accepted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC-EDGE: a `model` key on a codex/gemini entry is accepted (ignored by
    those lanes); the guard is claude-only, so back-compat holds."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    peers:\n"
        '      - provider: codex\n        model: "zai,glm-5.2"\n'
        "    peer_identity: fno-peer-bot\n",
    )
    assert settings.review.peers == [
        {"provider": "codex", "model": "zai,glm-5.2"}
    ]


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
    assert settings.review.optional_apps == ["chatgpt-codex-connector"]


def test_optional_apps_scalar_coerces(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    optional_apps: chatgpt-codex-connector\n",
    )
    assert settings.review.optional_apps == ["chatgpt-codex-connector"]


def test_optional_apps_defaults_empty(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Absent -> the RAW field stays None (the lane probe reads its
    truthiness) while the RESOLVED list carries the built-in honored-if-present
    logins, independent of the required gate."""
    from fno.config import DEFAULT_OPTIONAL_APPS, resolved_optional_apps

    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.review.optional_apps is None
    assert resolved_optional_apps(settings.review) == list(DEFAULT_OPTIONAL_APPS)


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
    assert settings.review.github_apps == ["123"]


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
    assert settings.review.github_apps is None


def test_optional_apps_scalar_and_mapping_parity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    numeric = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    optional_apps: 123\n",
    )
    assert numeric.review.optional_apps == ["123"]
    mapping = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    optional_apps: {a: b}\n",
    )
    assert mapping.review.optional_apps == []


# --- reviewers: local-attestation gate (x-e703, Phase 2) ---


def test_reviewers_defaults_empty(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Absent -> [] (no reviewers gate)."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.review.reviewers == []


def test_reviewers_list(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A one-entry reviewers list is exposed verbatim (AC2-HP)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    reviewers:\n      - sigma\n",
    )
    assert settings.review.reviewers == ["sigma"]


def test_reviewers_scalar_coerces(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    reviewers: sigma\n",
    )
    assert settings.review.reviewers == ["sigma"]


def test_reviewers_strips_leading_slash(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """`/code-review` and `code-review` are the same reviewer (slash stripped)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    reviewers: [/code-review, declare]\n",
    )
    assert settings.review.reviewers == ["code-review", "declare"]


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


# --- agent_harnesses rename + agent_providers alias ---


def test_agent_harnesses_canonical_map(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """agent_harnesses is read verbatim and mirrored onto the legacy alias."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    agent_harnesses:\n"
        "      code-reviewer: codex\n",
    )
    assert settings.review.agent_harnesses == {"code-reviewer": "codex"}
    assert settings.review.agent_providers == {"code-reviewer": "codex"}


def test_agent_providers_aliases_agent_harnesses(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A legacy agent_providers-only config populates agent_harnesses (AC6-HP)."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    agent_providers:\n"
        "      code-reviewer: codex\n",
    )
    assert settings.review.agent_harnesses == {"code-reviewer": "codex"}
    assert settings.review.agent_providers == settings.review.agent_harnesses


def test_agent_harnesses_wins_over_agent_providers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Both set: canonical wins and the two are never merged."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n"
        "    agent_harnesses: {code-reviewer: codex}\n"
        "    agent_providers: {silent-failure-hunter: gemini}\n",
    )
    assert settings.review.agent_harnesses == {"code-reviewer": "codex"}
    assert "silent-failure-hunter" not in settings.review.agent_harnesses


def test_agent_harnesses_absent_is_empty(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Neither key set stays an empty map, so cross-model stays off."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\nconfig:\n  review: {}\n")
    assert settings.review.agent_harnesses == {}
    assert settings.review.agent_providers == {}


def test_agent_harnesses_malformed_degrades_to_empty(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A non-mapping value degrades to {} rather than raising out of the load."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    agent_harnesses: nonsense\n",
    )
    assert settings.review.agent_harnesses == {}


# --- AC6: a non-author GitHub approval is a sufficient producer ---


def test_github_approval_satisfies_defaults_on(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Absent key -> True: the one producer a stranger's GitHub project emits
    with no footnote machinery ships ON."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\nconfig:\n  review: {}\n")
    assert settings.review.github_approval_satisfies is True


def test_github_approval_satisfies_explicit_false(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """An explicit false keeps today's recorded-but-never-counted behavior."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    github_approval_satisfies: false\n",
    )
    assert settings.review.github_approval_satisfies is False


def test_github_approval_registry_row_documents_the_key() -> None:
    """The registry carries the row, so `fno config` teaches the knob."""
    from fno.config.registry import FIELD_META

    assert "review.github_approval_satisfies" in FIELD_META


def _verdict(name: str, author_approval: bool | None) -> dict:
    row = {
        "producer": "github_app",
        "name": name,
        "verdict": "reviewed",
        "human_approval": True,
        "reviewed_sha": "h1",
        "freshness": "fresh",
    }
    if author_approval is not None:
        row["author_approval"] = author_approval
    return row


def test_ac6_hp_non_author_approval_derives_reviewed() -> None:
    """bob's approval on alice's PR, flag on: the state derivation counts it."""
    from fno.pr._reviews import _derive_review_state

    assert (
        _derive_review_state("covered", [_verdict("bob", False)], set(), True) == "reviewed"
    )


def test_ac6_err_author_approval_is_never_counted() -> None:
    """alice approving her own PR stays recorded and never derives reviewed,
    so a run where no approval was collected at all cannot pass this either."""
    from fno.pr._reviews import _derive_review_state

    assert (
        _derive_review_state("covered", [_verdict("alice", True)], set(), True)
        == "unreviewed"
    )


def test_ac6_edge_flag_off_keeps_todays_exclusion() -> None:
    """Flag off: bob is still on the verdict list (recorded) and still
    excluded from the count - exactly today's behavior."""
    from fno.pr._reviews import _derive_review_state

    assert (
        _derive_review_state("covered", [_verdict("bob", False)], set(), False)
        == "unreviewed"
    )


def test_ac6_pre_field_row_stays_excluded_even_with_flag_on() -> None:
    """A verdict serialized before author_approval existed carries no marker;
    the absent marker reads as the exclude side (fail-closed), flag or not."""
    from fno.pr._reviews import _derive_review_state

    assert (
        _derive_review_state("covered", [_verdict("bob", None)], set(), True)
        == "unreviewed"
    )


# --- AC7: the round budget config ---


def test_max_rounds_defaults_to_two(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\nconfig:\n  review: {}\n")
    assert settings.review.max_rounds == 2


def test_max_rounds_explicit_value(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  review:\n    max_rounds: 3\n",
    )
    assert settings.review.max_rounds == 3


def test_max_rounds_below_one_fails_loud(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Zero or negative fails the load, never silently zeroes the budget - a
    missing cap would let every round refund into an unbounded review."""
    with pytest.raises(Exception):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  review:\n    max_rounds: 0\n",
        )


def test_max_rounds_registry_row_documents_the_key() -> None:
    from fno.config.registry import FIELD_META

    assert "review.max_rounds" in FIELD_META


# --- review.posture: the nine-rung ladder ---


def _review(content: str):
    """A ReviewBlock parsed from a review-block YAML fragment."""
    from fno.config import ReviewBlock

    import yaml as _yaml

    return ReviewBlock(**_yaml.safe_load(content))


def test_posture_defaults_unset_and_bare_install_is_default_floor() -> None:
    """No posture leaf anywhere: the shipped default is self_review (rung 3),
    source=default, and NO migration command (nothing was inferred)."""
    from fno.config import resolve_review_posture

    block = _review("{}")
    assert block.posture is None
    resolved = resolve_review_posture(block)
    assert resolved.value == "self_review"
    assert resolved.rank == 3
    assert resolved.source == "default"
    assert resolved.migration is None
    assert not resolved.automerge_blocked


def test_posture_explicit_leaf_wins() -> None:
    from fno.config import resolve_review_posture

    resolved = resolve_review_posture(_review("posture: peer_review\n"))
    assert resolved.value == "peer_review"
    assert resolved.rank == 6
    assert resolved.source == "explicit"
    assert resolved.migration is None


def test_posture_typo_fails_loud() -> None:
    """A misspelled rung must never read as unset (unset infers and can only
    land on a real rung; a typo falling through would ship a different rung
    than the operator wrote)."""
    import pytest as _pytest

    from fno.config import ReviewBlock

    with _pytest.raises(Exception, match="not one of"):
        ReviewBlock(posture="self_reviewk")


def test_posture_legacy_github_app_infers_self_and_github() -> None:
    """The shipped self_review_required floor composes with the App gate."""
    from fno.config import resolve_review_posture

    resolved = resolve_review_posture(
        _review("github_apps: [chatgpt-codex-connector]\n")
    )
    assert resolved.value == "self_and_github"
    assert resolved.source == "legacy"
    assert resolved.migration == (
        "fno config set review.posture self_and_github --local"
    )
    assert resolved.signals == ("github_apps", "self_review_required")


def test_posture_legacy_declared_none_infers_tests_pass() -> None:
    """An explicit [] gate is the declared PR+CI-only path: rung 2, not 1."""
    from fno.config import resolve_review_posture

    resolved = resolve_review_posture(_review("required_bots: []\n"))
    assert resolved.value == "tests_pass"
    assert resolved.source == "legacy"
    assert resolved.automerge_blocked


def test_posture_legacy_explicit_floor_optout_infers_no_review() -> None:
    """An explicit self_review_required=false with no other lane read as
    no-gate before the ladder existed; the inference preserves the opt-out
    visibly instead of silently hardening it."""
    from fno.config import resolve_review_posture

    resolved = resolve_review_posture(_review("self_review_required: false\n"))
    assert resolved.value == "no_review"
    assert resolved.automerge_blocked
    assert resolved.migration == "fno config set review.posture no_review --local"


def test_posture_legacy_corroboration_is_ignored() -> None:
    """Origin never gates, so require_corroboration no longer feeds the
    legacy inference: setting it changes nothing the posture reports."""
    from fno.config import resolve_review_posture

    with_it = resolve_review_posture(_review("require_corroboration: true\n"))
    without = resolve_review_posture(_review("optout_ttl_minutes: 30\n"))
    assert with_it.value == without.value == "self_review"
    assert "require_corroboration" not in with_it.signals


def test_posture_legacy_peers_infers_self_and_peer() -> None:
    from fno.config import resolve_review_posture

    resolved = resolve_review_posture(_review("peers: [codex]\n"))
    assert resolved.value == "self_and_peer"
    assert resolved.migration == "fno config set review.posture self_and_peer --local"


def test_posture_legacy_github_plus_peers_infers_top_rung() -> None:
    from fno.config import resolve_review_posture

    resolved = resolve_review_posture(
        _review("github_apps: [codex]\npeers: [gemini]\n")
    )
    assert resolved.value == "self_github_and_peer"
    assert resolved.rank == 9


def test_posture_legacy_floor_off_downgrades_composition() -> None:
    """An explicit floor opt-out keeps the inference from claiming a self lane
    the config explicitly declined."""
    from fno.config import resolve_review_posture

    resolved = resolve_review_posture(
        _review("github_apps: [codex]\nself_review_required: false\n")
    )
    assert resolved.value == "github_review"


def test_posture_sigma_and_declare_never_count_as_a_self_lane() -> None:
    """sigma is retired (refuses at init) and declare is a self-cert; neither
    may make a legacy config LOOK reviewed."""
    from fno.config import resolve_review_posture

    resolved = resolve_review_posture(_review("reviewers: [sigma, declare]\n"))
    assert resolved.value == "self_review"
    assert resolved.source == "default"
    assert "reviewers" not in resolved.signals


def test_posture_descriptors_cover_nine_rungs_with_costs() -> None:
    """The ladder is exactly nine rungs, ranked 1..9, each carrying the cost
    string a receipt prints."""
    from fno.config import REVIEW_POSTURES

    assert len(REVIEW_POSTURES) == 9
    ranks = sorted(d.rank for d in REVIEW_POSTURES.values())
    assert ranks == list(range(1, 10))
    for name, d in REVIEW_POSTURES.items():
        assert d.value == name
        assert d.cost
        assert d.summary


def test_require_corroboration_registry_row_names_the_deprecation() -> None:
    """The row teaches the retirement: the key loads but changes nothing."""
    from fno.config.registry import FIELD_META

    doc = FIELD_META["review.require_corroboration"].doc.casefold()
    assert "deprecated" in doc
    assert "origin never gates" in doc
