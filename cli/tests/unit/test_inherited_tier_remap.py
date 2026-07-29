"""A spawn must never inherit a foreign vendor's tier-alias remap.

Regression for the split-brain that produced a "live" receipt and a session
that died on turn one: a z.ai/GLM parent exports
``ANTHROPIC_DEFAULT_OPUS_MODEL=glm-5.2[1m]``, so ``spawn --model opus``
reached Anthropic asking for a GLM model id.
"""

import os
from types import SimpleNamespace

import pytest
from pydantic import ValidationError

from fno.adapters.providers.model import ProviderRecord
from fno.agents.account_env import SCRUB_AUTH_VARS
from fno.agents.model_routing import (
    ENV_SCRUB_VAR,
    MODEL_ENV_KEYS,
    TIER_ALIASES,
    TierRemapConflict,
    check_spawn_tier_remap,
    emit_env_scrub_warning,
    env_scrub_warning,
)
from fno.agents.rust_runtime import (
    _scrub_account_auth_at_seam,
    _warn_env_scrub_spawn,
    env_scrub_spawn_warning,
    inherited_tier_remap,
)

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


def test_a_coherent_anthropic_tier_pin_is_not_a_conflict():
    # Pinning a tier to a specific Anthropic model is a supported Claude Code
    # customization: endpoint and model still agree, so it must not refuse.
    for pinned in ("claude-opus-4-1", "claude-opus-5", "Claude-Opus-4-1"):
        env = {"ANTHROPIC_DEFAULT_OPUS_MODEL": pinned}
        assert inherited_tier_remap([*BASE, "--model", "opus"], env) is None, pinned
        check_spawn_tier_remap("claude", "opus", env=env)
    # A foreign vendor id is still a conflict.
    assert inherited_tier_remap([*BASE, "--model", "opus"], ZAI_ENV) is not None


def test_the_fable_tier_is_covered_everywhere():
    # `fable` is a live alias here (fno agents spawn --model fable), and it has
    # its own ANTHROPIC_DEFAULT_FABLE_MODEL. Missing it left the fable tier of a
    # routed worker resolving at Anthropic while every other tier ran on the
    # secondary provider, and let --model fable slip the guard.
    assert "fable" in TIER_ALIASES
    assert "ANTHROPIC_DEFAULT_FABLE_MODEL" in MODEL_ENV_KEYS
    assert "ANTHROPIC_DEFAULT_FABLE_MODEL" in SCRUB_AUTH_VARS
    env = {"ANTHROPIC_DEFAULT_FABLE_MODEL": "glm-5.2[1m]"}
    assert inherited_tier_remap([*BASE, "--model", "fable"], env) == (
        "fable",
        "glm-5.2[1m]",
    )


def test_routed_env_covers_every_tier_alias():
    # The set a route composes must equal the set of tiers, or a worker leaks
    # the uncovered tier back to Anthropic.
    assert set(MODEL_ENV_KEYS) == {"ANTHROPIC_MODEL"} | {
        f"ANTHROPIC_DEFAULT_{a.upper()}_MODEL" for a in TIER_ALIASES
    }


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


# ---- x-369c: auth=api_key recognizes ANTHROPIC_AUTH_TOKEN ----


def test_an_api_key_record_naming_an_anthropic_compatible_vendor_validates():
    # ANTHROPIC_AUTH_TOKEN is the credential Claude Code reads for a non-Anthropic
    # Anthropic-compatible endpoint (z.ai, DeepSeek), so an api_key record may
    # carry it instead of ANTHROPIC_API_KEY.
    record = ProviderRecord(
        id="zai",
        name="zai",
        cli="claude",
        auth="api_key",
        env={
            "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
            "ANTHROPIC_AUTH_TOKEN": "${ENV:ZAI_API_KEY}",
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2[1m]",
        },
    )
    assert record.auth == "api_key"


def test_an_api_key_record_with_no_recognized_credential_still_fails():
    # The validator must not become a rubber stamp: an env block with endpoint
    # and model vars but no credential is still rejected.
    with pytest.raises(ValidationError):
        ProviderRecord(
            id="zai",
            name="zai",
            cli="claude",
            auth="api_key",
            env={
                "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2[1m]",
            },
        )


# ---- x-d532: warn (never refuse) when spawning under env scrub ----


def test_env_scrub_warning_fires_when_var_set_and_a_mode_is_pinned():
    # --yolo is a stated permission intent; the var silently overrides it, and
    # the warning names both consequences plus the opt-out.
    msg = env_scrub_spawn_warning([*BASE, "--yolo"], env={ENV_SCRUB_VAR: "1"})
    assert msg is not None
    assert ENV_SCRUB_VAR in msg
    assert "ANTHROPIC_AUTH_TOKEN" in msg
    assert "ANTHROPIC_BASE_URL" in msg
    assert "--permission-mode" in msg
    assert f"{ENV_SCRUB_VAR}=0" in msg


def test_env_scrub_warning_fires_for_an_explicit_permission_mode_flag():
    msg = env_scrub_spawn_warning(
        [*BASE, "--permission-mode", "bypassPermissions"], env={ENV_SCRUB_VAR: "1"}
    )
    assert msg is not None


def test_env_scrub_warning_is_absent_when_the_var_is_unset():
    assert env_scrub_spawn_warning([*BASE, "--yolo"], env={}) is None


@pytest.mark.parametrize("val", ["1", "true", "TRUE", "yes", "on", "scary"])
def test_env_scrub_warning_treats_truthy_values_as_set(val):
    assert env_scrub_spawn_warning([*BASE, "--yolo"], env={ENV_SCRUB_VAR: val}) is not None


@pytest.mark.parametrize("val", ["", "0", "false", "FALSE", "off", "no"])
def test_env_scrub_warning_treats_off_values_as_unset(val):
    assert env_scrub_spawn_warning([*BASE, "--yolo"], env={ENV_SCRUB_VAR: val}) is None


def test_env_scrub_warning_is_absent_when_no_permission_mode_is_named():
    # Narrow, not unconditional: a spawn that pins no mode is left alone.
    assert env_scrub_spawn_warning(BASE, env={ENV_SCRUB_VAR: "1"}) is None


def test_env_scrub_warning_is_absent_for_a_non_claude_harness():
    # The var is Claude Code specific; a codex spawn is unaffected.
    assert (
        env_scrub_spawn_warning(
            [*BASE, "--yolo", "-H", "codex"], env={ENV_SCRUB_VAR: "1"}
        )
        is None
    )


def test_env_scrub_warning_is_not_a_refusal(monkeypatch, capsys):
    # A warning, never a refusal: the seam prints and returns normally even when
    # the message fires (unlike _refuse_inherited_tier_remap, which exits 2).
    monkeypatch.setenv(ENV_SCRUB_VAR, "1")
    _warn_env_scrub_spawn([*BASE, "--yolo"])  # must not raise
    captured = capsys.readouterr()
    assert ENV_SCRUB_VAR in captured.err


def test_env_scrub_warning_structured_form_uses_the_resolved_provider():
    # The in-process spawn APIs hand the detector the provider they resolved, so
    # it judges the spawn on what it actually is, not a guessed claude.
    msg = env_scrub_warning("claude", permission_pinned=True, env={ENV_SCRUB_VAR: "1"})
    assert msg is not None
    assert (
        env_scrub_warning("codex", permission_pinned=True, env={ENV_SCRUB_VAR: "1"})
        is None
    )
    assert env_scrub_warning("claude", permission_pinned=False, env={ENV_SCRUB_VAR: "1"}) is None
    assert env_scrub_warning("claude", permission_pinned=True, env={}) is None


def test_env_scrub_spawn_warning_resolves_the_invoking_harness():
    # A bare spawn (no -H) defaults to the INVOKING harness, not claude: under a
    # codex session the scrub var is irrelevant, so the claude-specific warning
    # must not false-positive.
    codex_env = {ENV_SCRUB_VAR: "1", "CODEX_THREAD_ID": "abc-123"}
    assert env_scrub_spawn_warning([*BASE, "--yolo"], env=codex_env) is None
    # An explicit -H codex wins over an invoking claude session too.
    assert (
        env_scrub_spawn_warning(
            [*BASE, "--yolo", "-H", "codex"],
            env={ENV_SCRUB_VAR: "1", "CLAUDE_CODE_SESSION_ID": "abc-123"},
        )
        is None
    )
    # Under a claude invoking session the warning still fires.
    claude_env = {ENV_SCRUB_VAR: "1", "CLAUDE_CODE_SESSION_ID": "abc-123"}
    assert env_scrub_spawn_warning([*BASE, "--yolo"], env=claude_env) is not None


def test_emit_env_scrub_warning_prints_and_never_refuses(capsys):
    # The in-process spawn APIs (dispatch_spawn / dispatch_spawn_pane) call this
    # emitter, so the warning reaches every reachable path, not just the seam.
    emit_env_scrub_warning("claude", permission_pinned=True, env={ENV_SCRUB_VAR: "1"})
    assert ENV_SCRUB_VAR in capsys.readouterr().err
    # A non-claude provider is silent, and neither case raises (never a refusal).
    emit_env_scrub_warning("codex", permission_pinned=True, env={ENV_SCRUB_VAR: "1"})
    assert capsys.readouterr().err == ""


if __name__ == "__main__":
    for name, fn in sorted(globals().items()):
        if name.startswith("test_"):
            fn()
            print(f"ok {name}")
