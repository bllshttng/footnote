"""A spawn must never inherit a foreign vendor's tier-alias remap.

Regression for the split-brain that produced a "live" receipt and a session
that died on turn one: a z.ai/GLM parent exports
``ANTHROPIC_DEFAULT_OPUS_MODEL=glm-5.2[1m]``, so ``spawn --model opus``
reached Anthropic asking for a GLM model id.
"""

import os
from types import SimpleNamespace

import pytest

from fno.agents.account_env import SCRUB_AUTH_VARS
from fno.agents.model_routing import TierRemapConflict, check_spawn_tier_remap
from fno.agents.rust_runtime import _scrub_account_auth_at_seam, inherited_tier_remap

ZAI_ENV = {
    "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
    "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2[1m]",
    "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-5.2[1m]",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.5-air",
}

BASE = ["spawn", "--name", "w", "--substrate", "bg", "hi"]


def test_unrouted_tier_alias_under_a_remap_is_caught():
    assert inherited_tier_remap([*BASE, "--model", "opus"], ZAI_ENV) == (
        "opus",
        "glm-5.2[1m]",
    )
    assert inherited_tier_remap([*BASE, "--model=sonnet"], ZAI_ENV) is not None


def test_a_composed_route_is_never_ambiguous():
    # Each composes endpoint+auth+model as one unit before any worker is born
    # (-P/--route are fail-closed at the CLI; --account is resolved+scrubbed at
    # the seam), so the ambient remap is irrelevant and the spawn must proceed.
    for flags in (
        ["-P", "zai", "--model", "glm-5.2[1m]"],
        ["--route", "zai/glm-5.2", "--model", "opus"],
        ["--account", "makers", "--model", "opus"],
    ):
        assert inherited_tier_remap([*BASE, *flags], ZAI_ENV) is None, flags


def test_a_role_exempts_only_once_it_resolves_to_a_real_route(monkeypatch):
    # resolve_route is fail-SAFE: a protected role, a disabled block, an
    # unconfigured provider or a missing key all return None and leave the
    # ambient env untouched, so mentioning --role must not exempt on its own.
    args = [*BASE, "--role", "tidy", "--model", "opus"]

    monkeypatch.setattr(
        "fno.agents.model_routing.resolve_route", lambda *a, **k: None
    )
    assert inherited_tier_remap(args, ZAI_ENV) == ("opus", "glm-5.2[1m]")

    monkeypatch.setattr(
        "fno.agents.model_routing.resolve_route",
        lambda *a, **k: {"ANTHROPIC_MODEL": "glm-5.2"},
    )
    assert inherited_tier_remap(args, ZAI_ENV) is None


def test_in_process_spawn_apis_enforce_the_same_invariant():
    # The CLI seam is one of several reachable paths; dispatch_spawn and
    # dispatch_spawn_pane accept `model` directly and must fail closed too.
    with pytest.raises(TierRemapConflict):
        check_spawn_tier_remap("claude", "opus", env=ZAI_ENV)
    # A resolved route or account overlay composes the whole thing: exempt.
    check_spawn_tier_remap("claude", "opus", env=ZAI_ENV, route_env={"A": "b"})
    check_spawn_tier_remap("claude", "opus", env=ZAI_ENV, account_env={"A": "b"})
    # Non-claude providers never read these vars.
    check_spawn_tier_remap("codex", "opus", env=ZAI_ENV)
    # A clean parent, and a vendor model id that is not a tier alias.
    check_spawn_tier_remap("claude", "opus", env={})
    check_spawn_tier_remap("claude", "glm-5.2[1m]", env=ZAI_ENV)


def test_clean_parent_and_non_claude_harness_are_untouched():
    assert inherited_tier_remap([*BASE, "--model", "opus"], {}) is None
    # A vendor model id is not a tier alias -- nothing remaps it.
    assert inherited_tier_remap([*BASE, "--model", "glm-5.2[1m]"], ZAI_ENV) is None
    # The remap vars are claude-only.
    assert (
        inherited_tier_remap([*BASE, "-H", "codex", "--model", "opus"], ZAI_ENV) is None
    )


def test_payload_tokens_after_argv_are_not_read_as_flags():
    args = [*BASE, "--argv", "--model", "opus"]
    assert inherited_tier_remap(args, ZAI_ENV) is None


def test_account_scrub_covers_every_model_remap_var():
    # --account re-points endpoint + auth; leaving a foreign vendor's model
    # aliases behind is the same half-composition, one layer down.
    for var in ZAI_ENV:
        assert var in SCRUB_AUTH_VARS, var
    assert "ANTHROPIC_MODEL" in SCRUB_AUTH_VARS


def test_account_spawn_scrubs_at_the_seam_so_rust_inherits_it(monkeypatch):
    # An --account spawn on --substrate bg auto-routes to the Rust client, which
    # has no ANTHROPIC_* handling; route_to_rust execs with os.environ, so the
    # seam is the only edit that lane sees.
    for k, v in ZAI_ENV.items():
        monkeypatch.setenv(k, v)
    monkeypatch.setattr(
        "fno.agents.account_env.resolve_account_overlay",
        lambda aid: SimpleNamespace(env={"CLAUDE_CONFIG_DIR": "/tmp/x"}),
    )
    _scrub_account_auth_at_seam([*BASE, "--account", "makers", "--model", "opus"])
    assert not [k for k in SCRUB_AUTH_VARS if k in os.environ]
    assert os.environ["CLAUDE_CONFIG_DIR"] == "/tmp/x"


def test_seam_resolves_the_account_before_scrubbing(monkeypatch):
    # An api_key record may reference the ambient env
    # (ANTHROPIC_API_KEY = "${ENV:ANTHROPIC_API_KEY}") and resolve_env_value
    # reads os.environ, so scrubbing first would delete the source value and
    # make a previously valid --account spawn unresolvable.
    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-from-parent")
    seen = {}

    def fake_resolve(account_id):
        seen["account"] = account_id
        seen["key_at_resolve"] = os.environ.get("ANTHROPIC_API_KEY")
        return SimpleNamespace(env={"ANTHROPIC_API_KEY": "sk-resolved"})

    monkeypatch.setattr(
        "fno.agents.account_env.resolve_account_overlay", fake_resolve
    )
    _scrub_account_auth_at_seam([*BASE, "--account=acct", "--model", "opus"])
    assert seen["account"] == "acct"
    assert seen["key_at_resolve"] == "sk-from-parent"
    assert os.environ["ANTHROPIC_API_KEY"] == "sk-resolved"


def test_seam_leaves_env_untouched_when_the_account_cannot_resolve(monkeypatch):
    # The downstream resolver owns the refusal receipt; the seam must not
    # pre-empt it by half-scrubbing on the way out.
    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-from-parent")
    monkeypatch.setenv("ANTHROPIC_BASE_URL", ZAI_ENV["ANTHROPIC_BASE_URL"])

    def boom(account_id):
        raise ValueError("unresolvable")

    monkeypatch.setattr("fno.agents.account_env.resolve_account_overlay", boom)
    _scrub_account_auth_at_seam([*BASE, "--account", "acct", "--model", "opus"])
    assert os.environ["ANTHROPIC_API_KEY"] == "sk-from-parent"
    assert os.environ["ANTHROPIC_BASE_URL"] == ZAI_ENV["ANTHROPIC_BASE_URL"]


def test_seam_scrub_leaves_a_non_account_spawn_alone(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_BASE_URL", ZAI_ENV["ANTHROPIC_BASE_URL"])
    _scrub_account_auth_at_seam([*BASE, "-P", "zai", "--model", "glm-5.2"])
    assert os.environ["ANTHROPIC_BASE_URL"] == ZAI_ENV["ANTHROPIC_BASE_URL"]


if __name__ == "__main__":
    for name, fn in sorted(globals().items()):
        if name.startswith("test_"):
            fn()
            print(f"ok {name}")
