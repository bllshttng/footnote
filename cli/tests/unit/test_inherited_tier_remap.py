"""A spawned worker must never run on a model nobody routed for it.

The spawn axis flags are retired, so the CLI can no longer type a tier alias:
the in-process spawn APIs are the remaining surface that accepts ``model``
directly, and they fail closed via ``check_spawn_tier_remap`` when the alias
would resolve against an inherited foreign vendor's remap (the split-brain
that produced a "live" receipt and a session dead on turn one).
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
    base_url_is_anthropic,
    check_spawn_tier_remap,
    emit_env_scrub_warning,
    env_scrub_warning,
    incoherent_model_env,
    incoherent_model_env_notice,
    incoherent_model_env_unset_args,
    overlay_restores_model_env,
    scrub_incoherent_model_env,
    scrub_incoherent_model_env_and_notify,
    unrouted_model_keys,
)
from fno.agents.rust_runtime import _scrub_account_auth_at_seam

ZAI_ENV = {
    "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
    "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2[1m]",
    "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-5.2[1m]",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.5-air",
}

BASE = ["spawn", "--name", "w", "hi"]


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


def test_a_coherent_anthropic_tier_pin_is_not_a_conflict():
    # Pinning a tier to a specific Anthropic model is a supported Claude Code
    # customization: endpoint and model still agree, so it must not refuse.
    for pinned in ("claude-opus-4-1", "claude-opus-5", "Claude-Opus-4-1"):
        env = {"ANTHROPIC_DEFAULT_OPUS_MODEL": pinned}
        check_spawn_tier_remap("claude", "opus", env=env)


def test_the_fable_tier_is_covered_everywhere():
    # `fable` is a live tier alias, and it has its own
    # ANTHROPIC_DEFAULT_FABLE_MODEL. Missing it left the fable tier of a routed
    # worker resolving at Anthropic while every other tier ran on the secondary
    # provider.
    assert "fable" in TIER_ALIASES
    assert "ANTHROPIC_DEFAULT_FABLE_MODEL" in MODEL_ENV_KEYS
    assert "ANTHROPIC_DEFAULT_FABLE_MODEL" in SCRUB_AUTH_VARS


def test_routed_env_covers_every_tier_alias():
    # The set a route composes must equal the set of tiers, or a worker leaks
    # the uncovered tier back to Anthropic.
    assert set(MODEL_ENV_KEYS) == {"ANTHROPIC_MODEL"} | {
        f"ANTHROPIC_DEFAULT_{a.upper()}_MODEL" for a in TIER_ALIASES
    }


def test_account_scrub_covers_every_model_remap_var():
    # --account re-points endpoint + auth; leaving a foreign vendor's model
    # aliases behind is the same half-composition, one layer down.
    for var in ZAI_ENV:
        assert var in SCRUB_AUTH_VARS, var
    assert "ANTHROPIC_MODEL" in SCRUB_AUTH_VARS


def test_account_spawn_scrubs_at_the_seam_so_rust_inherits_it(monkeypatch):
    # An --account spawn on the headless lane auto-routes to the Rust client,
    # which has no ANTHROPIC_* handling; route_to_rust execs with os.environ,
    # so the seam is the only edit that lane sees.
    for k, v in ZAI_ENV.items():
        monkeypatch.setenv(k, v)
    monkeypatch.setattr(
        "fno.agents.account_env.resolve_account_overlay",
        lambda aid: SimpleNamespace(env={"CLAUDE_CONFIG_DIR": "/tmp/x"}),
    )
    _scrub_account_auth_at_seam([*BASE, "--account", "makers"])
    assert not [k for k in SCRUB_AUTH_VARS if k in os.environ]
    assert os.environ["CLAUDE_CONFIG_DIR"] == "/tmp/x"


def test_seam_clears_a_stale_launch_account_on_an_unaccounted_spawn(monkeypatch):
    # A worker spawned under --account carries FNO_LAUNCH_ACCOUNT in its env.
    # When THAT worker spawns a child with no account of its own, the seam must
    # clear the inherited id: the Rust mint reads it first, so a stale value
    # stamps the parent's account onto a row that pinned nothing.
    monkeypatch.setenv("FNO_LAUNCH_ACCOUNT", "parent-acct")
    _scrub_account_auth_at_seam([*BASE])
    assert "FNO_LAUNCH_ACCOUNT" not in os.environ


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
    _scrub_account_auth_at_seam([*BASE, "--account=acct"])
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
    _scrub_account_auth_at_seam([*BASE, "--account", "acct"])
    assert os.environ["ANTHROPIC_API_KEY"] == "sk-from-parent"
    assert os.environ["ANTHROPIC_BASE_URL"] == ZAI_ENV["ANTHROPIC_BASE_URL"]


def test_seam_scrub_leaves_a_non_account_spawn_alone(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_BASE_URL", ZAI_ENV["ANTHROPIC_BASE_URL"])
    _scrub_account_auth_at_seam([*BASE, "--route", "zai/glm-5.2"])
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


# ---- x-d532: warn (never refuse) when an in-process spawn pins a mode under env scrub ----


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


def test_emit_env_scrub_warning_prints_and_never_refuses(capsys):
    # The in-process spawn APIs (dispatch_spawn / dispatch_spawn_pane) call this
    # emitter, so the warning reaches every reachable path.
    emit_env_scrub_warning("claude", permission_pinned=True, env={ENV_SCRUB_VAR: "1"})
    assert ENV_SCRUB_VAR in capsys.readouterr().err
    # A non-claude provider is silent, and neither case raises (never a refusal).
    emit_env_scrub_warning("codex", permission_pinned=True, env={ENV_SCRUB_VAR: "1"})
    assert capsys.readouterr().err == ""


# ---- x-4709: strip an incoherent inherited model env at every spawn seam ----

#: The operator's measured daemon-carried env: GLM names, no base URL.
POISON_ENV = {
    "ANTHROPIC_MODEL": "glm-5.2[1m]",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.5-air",
}


def test_incoherent_model_env_returns_every_poisoned_var():
    # Every offender, not the first: the measured carrier poisoned more than
    # one var, and a repair that drops half of them ships the same bug on the
    # tiers it missed.
    assert dict(incoherent_model_env(POISON_ENV)) == POISON_ENV


def test_a_real_route_is_never_stripped():
    # A foreign base URL serves those model ids, so a composed route must reach
    # the child unchanged (AC3). The route case never even enters the strip.
    routed = {**POISON_ENV, "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic"}
    assert incoherent_model_env(routed) == ()
    assert base_url_is_anthropic(routed) is False


def test_a_pinned_anthropic_tier_is_coherent():
    # Pinning a tier to a specific Anthropic model is a supported customization
    # (the is_anthropic_model contract), not a conflict (AC5).
    assert incoherent_model_env({"ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus-4-1"}) == ()


def test_bedrock_and_vertex_lanes_are_coherent():
    # Those lanes serve Anthropic models under ids that do not start with
    # "claude-" and leave the base URL unset, so the question does not apply
    # (AC4). A pre-existing false positive on the single-var detector.
    bedrock_env = {
        "CLAUDE_CODE_USE_BEDROCK": "1",
        "ANTHROPIC_MODEL": "us.anthropic.claude-sonnet-4-20250514-v1:0",
    }
    assert incoherent_model_env(bedrock_env) == ()
    assert incoherent_model_env({**bedrock_env, "CLAUDE_CODE_USE_VERTEX": "1"}) == ()
    # An explicit off-word is not a lane opt-in.
    off = {"CLAUDE_CODE_USE_BEDROCK": "0", **POISON_ENV}
    assert dict(incoherent_model_env(off)) == POISON_ENV


def test_a_lookalike_host_is_a_foreign_endpoint():
    # notanthropic.com must not match the anthropic.com host rule (the
    # substring trap): the endpoint serves the model, so nothing is dropped.
    lookalike = {**POISON_ENV, "ANTHROPIC_BASE_URL": "https://notanthropic.com/api"}
    assert incoherent_model_env(lookalike) == ()
    assert base_url_is_anthropic(lookalike) is False


def test_a_proxy_url_with_an_embedded_url_resolves_the_proxys_own_host():
    # base_url_host splits on the FIRST "://" (split("://", 1)[-1]): a proxy
    # URL carrying a second URL in its path
    # (https://gateway.corp.com/proxy/https://api.anthropic.com) is a foreign
    # endpoint at gateway.corp.com, not api.anthropic.com. This pins the
    # behavior the Rust mirror's rsplit variant got wrong - the two
    # implementations must agree, not just share a var-name list.
    proxied = {
        **POISON_ENV,
        "ANTHROPIC_BASE_URL": "https://gateway.corp.com/proxy/https://api.anthropic.com",
    }
    assert base_url_is_anthropic(proxied) is False
    assert incoherent_model_env(proxied) == ()


def test_dict_scrub_and_argv_args_drop_the_same_names():
    # Seam parity asserted on the resulting NAME SETS, not on both calling one
    # helper: asserting shared plumbing pins the call, not the destination.
    env = {**POISON_ENV, "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-5.2[1m]"}
    scrubbed = dict(env)
    dropped_by_dict = scrub_incoherent_model_env(scrubbed)
    argv = incoherent_model_env_unset_args(env)
    dropped_by_argv = {name for i, name in enumerate(argv) if i % 2 == 1}
    assert set(dropped_by_dict) == dropped_by_argv == {
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
    }
    assert all(flag == "-u" for i, flag in enumerate(argv) if i % 2 == 0)
    assert not [k for k in dropped_by_dict if k in scrubbed]
    # A coherent env scrubs nothing and builds a stable empty argv.
    assert scrub_incoherent_model_env(dict(ZAI_ENV)) == ()
    assert incoherent_model_env_unset_args(ZAI_ENV) == []


def test_unrouted_keys_name_every_claim_coherent_or_not():
    # The unrouted clear is not the incoherence predicate: a coherent pin is
    # still a claim the launching shell made, and an unrouted child must carry
    # no claim at all. Whitespace-only is no claim.
    env = {
        "ANTHROPIC_MODEL": "claude-opus-4-8",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL": "  ",
        "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2[1m]",
    }
    assert unrouted_model_keys(env) == (
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
    )


def test_a_hand_composed_foreign_route_is_never_cleared(capsys):
    # Over a foreign base the ambient tier remap is a working route the
    # operator built in their shell - the headless receipt resolves it as the
    # spawn's real model - so the unrouted clear stands down entirely.
    env = {
        "ANTHROPIC_BASE_URL": "https://api.z.ai/api/anthropic",
        "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2",
    }
    assert unrouted_model_keys(env) == ()
    untouched = dict(env)
    scrub_incoherent_model_env_and_notify(untouched, routed=False)
    assert untouched == env
    assert capsys.readouterr().err == ""


def test_an_unrouted_scrub_clears_a_coherent_claim_and_says_so(capsys):
    # A spawn composing no route inherits the shell's pin otherwise; the
    # notice is true whether or not a route was typed: it never
    # calls a pinned child unrouted nor prescribes the route it already used.
    env = {"ANTHROPIC_MODEL": "claude-opus-4-8"}
    dropped = scrub_incoherent_model_env_and_notify(env, routed=False)
    assert dropped == ()
    assert "ANTHROPIC_MODEL" not in env
    notice = capsys.readouterr().err
    assert "fno: cleared ANTHROPIC_MODEL" in notice
    assert "The child's model comes from its route" in notice
    assert "unrouted" not in notice


def test_a_routed_scrub_leaves_ambient_claims_to_the_overlay(capsys):
    # routed=True means an overlay re-supplies the tier right after; clearing
    # first would be harmless but the notice would lie ("falls back"), so the
    # unrouted clear stands down entirely.
    env = {"ANTHROPIC_MODEL": "claude-opus-4-8"}
    scrub_incoherent_model_env_and_notify(env, routed=True)
    assert env["ANTHROPIC_MODEL"] == "claude-opus-4-8"
    assert capsys.readouterr().err == ""


def test_unset_args_extend_to_coherent_claims_only_when_unrouted():
    env = {"ANTHROPIC_MODEL": "claude-opus-4-8", "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.5-air"}
    argv = incoherent_model_env_unset_args(env, unrouted=True)
    names = {name for i, name in enumerate(argv) if i % 2 == 1}
    # Both causes in one argv, no duplicate pairs, coherent claim included.
    assert names == {"ANTHROPIC_MODEL", "ANTHROPIC_DEFAULT_HAIKU_MODEL"}
    assert argv.count("-u") == 2
    # Default behavior is unchanged: only the incoherent var.
    names_default = {
        name for i, name in enumerate(incoherent_model_env_unset_args(env))
        if i % 2 == 1
    }
    assert names_default == {"ANTHROPIC_DEFAULT_HAIKU_MODEL"}


def test_a_full_route_reaches_the_child_over_an_ambient_pin():
    # The other direction, positive: a composed route's model keys survive a
    # scrubbed ambient env unchanged - the clear removes claims, never routes.
    from fno.agents.account_env import compose_worker_credentials

    route = {
        "ANTHROPIC_BASE_URL": "https://open.bigmodel.cn/api/anthropic",
        "ANTHROPIC_AUTH_TOKEN": "sk-test",
        **{k: "glm-5.3[1m]" for k in MODEL_ENV_KEYS},
    }
    ambient = {
        "ANTHROPIC_MODEL": "claude-opus-4-8",
        "PATH": "/usr/bin",
    }
    composed, _ = compose_worker_credentials(None, route, ambient)
    for key in MODEL_ENV_KEYS:
        assert composed[key] == "glm-5.3[1m]"
    assert composed["PATH"] == "/usr/bin"


def test_the_notice_names_each_dropped_var():
    msg = incoherent_model_env_notice(["ANTHROPIC_MODEL", "ANTHROPIC_DEFAULT_HAIKU_MODEL"])
    for name in ("ANTHROPIC_MODEL", "ANTHROPIC_DEFAULT_HAIKU_MODEL"):
        assert name in msg
    # Both remedies, so the line is actionable without a doc open.
    assert "settings.json" in msg
    assert "restart the daemon" in msg


def test_overlay_restores_model_env_requires_an_actual_model_var():
    # A config-dir/managed-active account overlay (resolve_account_overlay's
    # "config-dir"/"active-slot" lanes) sets only CLAUDE_CONFIG_DIR - no
    # MODEL_ENV_KEYS entry - so the dropped var is never resupplied and the
    # child genuinely falls back to its account's own default. A caller that
    # treated "account_env is truthy" as "routed" printed a false claim.
    config_dir_only = {"CLAUDE_CONFIG_DIR": "/Users/x/.claude-alt"}
    assert overlay_restores_model_env(config_dir_only) is False
    assert overlay_restores_model_env(None, config_dir_only) is False

    # A real route, or an own-dir/api-key account overlay carrying its own
    # model pins, DOES resupply the tier.
    routed = {"ANTHROPIC_MODEL": "glm-5.3", "ANTHROPIC_BASE_URL": "https://api.z.ai"}
    assert overlay_restores_model_env(routed) is True
    assert overlay_restores_model_env(config_dir_only, routed) is True
    assert overlay_restores_model_env(None, None) is False


def test_the_notice_wording_matches_routed():
    dropped = ["ANTHROPIC_DEFAULT_HAIKU_MODEL"]
    unrouted_msg = incoherent_model_env_notice(dropped, routed=False)
    routed_msg = incoherent_model_env_notice(dropped, routed=True)
    assert "falls back to its account's own default" in unrouted_msg
    assert "receives that route's own model instead" in routed_msg


def test_hook_var_list_matches_model_env_keys():
    # The var list lives in two languages (the hook cannot import Python
    # without paying a subprocess at every SessionStart), so pin the hook's
    # list to MODEL_ENV_KEYS: a future tier added to TIER_ALIASES without
    # updating the hook fails here naming the missing var (AC7).
    import re
    from pathlib import Path

    hook = Path(__file__).resolve().parents[3] / "hooks" / "attest-model.sh"
    block = re.search(r"MODEL_ENV_VARS=\((.*?)\)", hook.read_text(), re.S)
    assert block is not None, "hooks/attest-model.sh has no MODEL_ENV_VARS list"
    hook_vars = set(re.findall(r"\b(ANTHROPIC_[A-Z_]+)\b", block.group(1)))
    assert hook_vars == set(MODEL_ENV_KEYS), (
        f"hook/model var drift: hook-only={sorted(hook_vars - set(MODEL_ENV_KEYS))} "
        f"model-only={sorted(set(MODEL_ENV_KEYS) - hook_vars)}"
    )


def test_rust_mirror_var_list_matches_model_env_keys():
    # The compiled client carries its own mirror (it cannot import Python)
    # and is reachable without the Python seam: a direct `fno-agents spawn`
    # and the loop runtime both spawn through it. Pin the mirror's var list
    # to MODEL_ENV_KEYS so a new tier cannot land in one list and not the
    # other.
    import re
    from pathlib import Path

    mirror = (
        Path(__file__).resolve().parents[3]
        / "crates/fno-agents/src/model_env_scrub.rs"
    )
    block = re.search(r"MODEL_ENV_KEYS[^=]*=\s*\[(.*?)\]", mirror.read_text(), re.S)
    assert block is not None, "model_env_scrub.rs has no MODEL_ENV_KEYS list"
    rust_vars = set(re.findall(r'"(ANTHROPIC_[A-Z_]+)"', block.group(1)))
    assert rust_vars == set(MODEL_ENV_KEYS), (
        f"rust/model var drift: rust-only={sorted(rust_vars - set(MODEL_ENV_KEYS))} "
        f"model-only={sorted(set(MODEL_ENV_KEYS) - rust_vars)}"
    )


def test_rust_mirror_has_a_settings_floor_writer():
    # The bg serving session is forked by the claude daemon with the DAEMON's
    # own env, so an env-only scrub never reaches it. The compiled client is
    # reachable without the Python front door (fno-agents spawn, the loop
    # runtime), so the mirror must carry its own --settings floor writer, not
    # just the env scrub.
    from pathlib import Path

    mirror = (
        Path(__file__).resolve().parents[3]
        / "crates/fno-agents/src/model_env_scrub.rs"
    )
    src = mirror.read_text()
    assert "pub fn write_scrub_settings" in src, (
        "model_env_scrub.rs lost the settings-floor writer: the daemon fork "
        "keeps the poisoned env on every Rust-lane bg spawn"
    )
    bg = (
        Path(__file__).resolve().parents[3]
        / "crates/fno-agents/src/claude_ask.rs"
    )
    assert "write_scrub_settings" in bg.read_text(), (
        "claude_ask.rs bg_create stopped floating the model-scrub settings file"
    )


def test_rust_mirror_notice_matches_python_notice():
    # The stderr remedy line exists once per language and cannot share code.
    # Pin its operative content (both fallback sentences and both remedies)
    # to what incoherent_model_env_notice prints, so the Rust door cannot
    # drift from the Python doors on what the operator is told to do.
    from pathlib import Path

    mirror = (
        Path(__file__).resolve().parents[3]
        / "crates/fno-agents/src/model_env_scrub.rs"
    )
    src = mirror.read_text()
    for phrase in (
        "falls back to its account's own default",
        "receives that route's own model instead",
        "settings.json",
        "restart the daemon",
    ):
        assert phrase in src, (
            f"model_env_scrub.rs notice lost '{phrase}': the Rust door now "
            "tells the operator something different from Python's "
            "incoherent_model_env_notice"
        )


if __name__ == "__main__":
    for name, fn in sorted(globals().items()):
        if name.startswith("test_"):
            fn()
            print(f"ok {name}")
