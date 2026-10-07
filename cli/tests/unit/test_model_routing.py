"""Unit tests for role-based model routing (x-d2fe).

One table test per family; each row is a distinct branch of the resolver.
The resolver contract lives on config.model_routing (providers / roles /
extra_env): routed roles need a key (fail-safe None + notice), production
roles never route, and the tier map layers per provider.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from fno.agents import model_routing as mr
from fno.config import ConfigBlock, ModelRoutingBlock, SettingsModel


def _settings(**block_kwargs: object) -> SettingsModel:
    return SettingsModel(
        config=ConfigBlock(model_routing=ModelRoutingBlock(**block_kwargs))
    )


def _collector() -> tuple[list[str], "object"]:
    notes: list[str] = []
    return notes, notes.append


# --- routed roles ---------------------------------------------------------------

def test_consolidate_routes_to_zai_anthropic_endpoint() -> None:
    route = mr.resolve_route(
        "consolidate", settings=_settings(), env={"ZAI_API_KEY": "zk-secret"}
    )
    assert route is not None
    assert route["ANTHROPIC_BASE_URL"] == "https://api.z.ai/api/anthropic"
    assert route["ANTHROPIC_AUTH_TOKEN"] == "zk-secret"
    assert route["ANTHROPIC_MODEL"] == "glm-5.3"
    assert route["ANTHROPIC_DEFAULT_OPUS_MODEL"] == "glm-5.3"
    assert route["ANTHROPIC_DEFAULT_SONNET_MODEL"] == "glm-5.3"
    assert route["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "glm-4.7"


# --- roles that must stay untouched ----------------------------------------------

def test_untouched_roles_return_none_rows() -> None:
    keyed = {"ZAI_API_KEY": "k"}
    for role in ("implement", "review-verdict", "compile"):
        assert mr.resolve_route(role, settings=_settings(), env=keyed) is None, role
    for role in (None, "", "   "):
        assert mr.resolve_route(role, settings=_settings(), env=keyed) is None, role


# --- config roles map, extra_env, providers --------------------------------------


def test_disabled_block_returns_none_even_for_routed_role() -> None:
    assert (
        mr.resolve_route(
            "tidy", settings=_settings(enabled=False), env={"ZAI_API_KEY": "k"}
        )
        is None
    )


def test_extra_env_is_merged_and_can_override_a_tier() -> None:
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(
            extra_env={
                "API_TIMEOUT_MS": "3000000",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.7-flash",
            }
        ),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is not None
    assert route["API_TIMEOUT_MS"] == "3000000"
    # extra_env is merged last, so it wins over the per-role model for that tier.
    assert route["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "glm-4.7-flash"
    assert route["ANTHROPIC_MODEL"] == "glm-5.3"


def test_second_provider_rows():
    # A second anthropic-protocol provider routes via its own endpoint; an
    # entry can override only base_url and keep the built-in protocol/key.
    deepseek = mr.resolve_route(
        "tidy",
        settings=_settings(
            providers={
                "deepseek": {
                    "protocol": "anthropic",
                    "base_url": "https://api.deepseek.com/anthropic",
                    "api_key_env": "DEEPSEEK_API_KEY",
                }
            },
            roles={"tidy": "deepseek,deepseek-chat"},
        ),
        env={"DEEPSEEK_API_KEY": "dsk"},
    )
    assert deepseek is not None
    assert deepseek["ANTHROPIC_BASE_URL"] == "https://api.deepseek.com/anthropic"
    assert deepseek["ANTHROPIC_AUTH_TOKEN"] == "dsk"
    assert deepseek["ANTHROPIC_MODEL"] == "deepseek-chat"
    # deepseek has no haiku_model, so the haiku tier keeps the role model.
    assert deepseek["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "deepseek-chat"

    override = mr.resolve_route(
        "tidy",
        settings=_settings(
            providers={"zai": {"base_url": "https://api.z.ai/api/coding/paas/v4"}},
        ),
        env={"ZAI_API_KEY": "k"},
    )
    assert override is not None
    assert override["ANTHROPIC_BASE_URL"] == "https://api.z.ai/api/coding/paas/v4"


def test_unknown_or_foreign_provider_falls_back_with_notice() -> None:
    notes, sink = _collector()
    assert mr.resolve_route(
        "tidy", settings=_settings(roles={"tidy": "mystery,model-x"}),
        env={"ZAI_API_KEY": "k"}, notice=sink,
    ) is None
    assert notes
    notes, sink = _collector()
    assert mr.resolve_route(
        "tidy",
        settings=_settings(
            providers={
                "oai": {
                    "protocol": "openai",
                    "base_url": "https://api.z.ai/api/coding/paas/v4",
                    "api_key_env": "ZAI_API_KEY",
                }
            },
            roles={"tidy": "oai,glm-5.2"},
        ),
        env={"ZAI_API_KEY": "k"},
        notice=sink,
    ) is None
    assert any("protocol" in n for n in notes)


# --- resolve_explicit_route --------------------------------------------------------

def test_explicit_route_rows():
    route = mr.resolve_explicit_route(
        "zai", "glm-5.2", settings=_settings(), env={"ZAI_API_KEY": "zk"}
    )
    assert route is not None
    assert route["ANTHROPIC_BASE_URL"] == "https://api.z.ai/api/anthropic"
    assert route["ANTHROPIC_AUTH_TOKEN"] == "zk"
    assert route["ANTHROPIC_MODEL"] == "glm-5.2"
    # An explicit peer opt-in is not role auto-routing: neither the disabled
    # global flag nor PROTECTED_ROLES applies (there is no role).
    route = mr.resolve_explicit_route(
        "zai", "glm-5.2", settings=_settings(enabled=False), env={"ZAI_API_KEY": "k"}
    )
    assert route is not None and route["ANTHROPIC_MODEL"] == "glm-5.2"


def test_explicit_route_no_key_fails_safe() -> None:
    notes, sink = _collector()
    route = mr.resolve_explicit_route(
        "zai", "glm-5.2", settings=_settings(), env={}, notice=sink
    )
    assert route is None
    assert any("skipping the peer" in n for n in notes)
    assert not any("primary" in n for n in notes)


# --- tier_models --------------------------------------------------------------------

def test_tier_map_rows():
    # (provider extra, spawn model, expect model_keys)
    base_keys = {
        "ANTHROPIC_MODEL": "glm-5.3",
        "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.3",
        "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-5.3",
        "ANTHROPIC_DEFAULT_FABLE_MODEL": "glm-5.3",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.7",
    }
    rows = [
        # Undeclared tiers keep the spawn model; opus moves; haiku keeps the
        # provider's folded-in default.
        (
            {"tier_models": {"opus": "glm-5.3[1m]"}},
            "glm-5.3-flash[1m]",
            {
                "ANTHROPIC_MODEL": "glm-5.3-flash[1m]",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-5.3-flash[1m]",
                "ANTHROPIC_DEFAULT_FABLE_MODEL": "glm-5.3-flash[1m]",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.3[1m]",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.7",
            },
        ),
        # Both set resolves to tier_models, asserted rather than left to order.
        (
            {"tier_models": {"haiku": "glm-4.7-air"}, "haiku_model": "glm-4.5-air"},
            "glm-5.3",
            {**base_keys, "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.7-air"},
        ),
        # A config tier_models.haiku overrides the BUILT-IN haiku_model default.
        (
            {"tier_models": {"haiku": "glm-4.7-air"}},
            "glm-5.3",
            {**base_keys, "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.7-air"},
        ),
        # Empty tier_models behaves as unset.
        (
            {"tier_models": {}},
            "glm-5.3",
            base_keys,
        ),
        # A provider haiku_model override alone wins for haiku only.
        (
            {"haiku_model": "glm-tiny"},
            "glm-5.3",
            {**base_keys, "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-tiny"},
        ),
    ]
    for extra, spawn, want in rows:
        route = mr.resolve_explicit_route(
            "zai", spawn, settings=_settings(providers={"zai": extra}), env={"ZAI_API_KEY": "k"}
        )
        assert route is not None
        got = {k: route[k] for k in mr.MODEL_ENV_KEYS}
        assert got == want, extra


def test_refuses_bare_tier_alias_on_foreign_endpoint() -> None:
    notes, sink = _collector()
    route = mr.resolve_explicit_route(
        "zai", "sonnet", settings=_settings(), env={"ZAI_API_KEY": "k"}, notice=sink
    )
    assert route is None
    assert any("sonnet" in n and "zai" in n for n in notes)


def test_materialized_settings_keep_floor_and_undeclared_tiers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("fno.paths.state_dir", lambda: tmp_path)
    route = mr.resolve_explicit_route(
        "zai",
        "glm-5.3-flash[1m]",
        settings=_settings(
            providers={"zai": {"tier_models": {"opus": "glm-5.3[1m]"}}}
        ),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is not None
    path = Path(mr.materialize_route_settings(route))
    try:
        import json

        env_out = json.loads(path.read_text(encoding="utf-8"))["env"]
    finally:
        path.unlink()
    from fno.agents.account_env import SCRUB_AUTH_VARS

    for var in SCRUB_AUTH_VARS:
        assert var in env_out
    assert env_out["ANTHROPIC_API_KEY"] == ""
    assert env_out["CLAUDE_CODE_OAUTH_TOKEN"] == ""
    assert env_out["ANTHROPIC_DEFAULT_SONNET_MODEL"] == "glm-5.3-flash[1m]"
    assert env_out["ANTHROPIC_DEFAULT_OPUS_MODEL"] == "glm-5.3[1m]"


# --- key resolution: env, env file, custom names ------------------------------------

def test_key_resolution_rows(tmp_path: Path) -> None:
    envf = tmp_path / "modelkit.env"
    envf.write_text("# comment\nZAI_API_KEY=from-file\n", encoding="utf-8")
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(providers={"zai": {"api_key_file": str(envf)}}),
        env={},
    )
    assert route is not None and route["ANTHROPIC_AUTH_TOKEN"] == "from-file"

    envf.write_text("ZAI_API_KEY=from-file\n", encoding="utf-8")
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(providers={"zai": {"api_key_file": str(envf)}}),
        env={"ZAI_API_KEY": "from-process"},
    )
    assert route is not None and route["ANTHROPIC_AUTH_TOKEN"] == "from-process"

    envf.write_text("export ZAI_API_KEY = spaced-and-exported\n", encoding="utf-8")
    route = mr.resolve_route(
        "consolidate",
        settings=_settings(providers={"zai": {"api_key_file": str(envf)}}),
        env={},
    )
    assert route is not None and route["ANTHROPIC_AUTH_TOKEN"] == "spaced-and-exported"

    route = mr.resolve_route(
        "consolidate",
        settings=_settings(providers={"zai": {"api_key_file": "/no/such/file.env"}}),
        env={},
    )
    assert route is None

    route = mr.resolve_route(
        "orient",
        settings=_settings(providers={"zai": {"api_key_env": "MY_GLM_KEY"}}),
        env={"MY_GLM_KEY": "alt"},
    )
    assert route is not None and route["ANTHROPIC_AUTH_TOKEN"] == "alt"


# --- protected roles ------------------------------------------------------------------

def test_protected_roles_never_route_rows(monkeypatch) -> None:
    assert "implement" in mr.PROTECTED_ROLES
    assert "review-verdict" in mr.PROTECTED_ROLES
    assert mr.PROTECTED_ROLE_FLOOR == "high"
    assert mr.resolve_route(
        "implement",
        settings=_settings(roles={"implement": "zai,glm-5.2"}),
        env={"ZAI_API_KEY": "k"},
    ) is None
    notices: list[str] = []
    assert mr.resolve_route(
        "implement",
        settings=_settings(roles={"implement": "zai,glm-5.2"}),
        env={"ZAI_API_KEY": "k"},
        notice=notices.append,
    ) is None
    assert any("protected-role(implement) floor=high" in n for n in notices)


# --- codex lane -------------------------------------------------------------------------

def _openai_settings(model: str = "glm-5.2", **extra: object) -> SettingsModel:
    prov = {
        "zai-openai": {
            "protocol": "openai",
            "base_url": "https://api.z.ai/api/coding/paas/v4",
            "api_key_env": "OPENAI_API_KEY",
            **extra,
        }
    }
    return _settings(providers=prov, roles={"tidy": f"zai-openai,{model}"})


def _pin_codex_config(
    tmp_path, monkeypatch, provider: str, body: str, *, role: str = "tidy"
) -> None:
    cfg = tmp_path / "config.toml"
    cfg.write_text(
        "[model_routing]\n"
        f'[model_routing.roles]\n{role} = "{provider},glm-5.2"\n'
        f"[model_routing.providers.{provider}]\n" + body,
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CONFIG", str(cfg))


requires_rust = pytest.mark.dev_build


@requires_rust
def test_codex_route_returns_config_and_env_for_openai_provider(tmp_path, monkeypatch) -> None:
    _pin_codex_config(
        tmp_path,
        monkeypatch,
        "zai-openai",
        'protocol = "openai"\n'
        'base_url = "https://api.z.ai/api/coding/paas/v4"\n'
        'api_key_env = "OPENAI_API_KEY"\n',
    )
    monkeypatch.setenv("OPENAI_API_KEY", "oai-key")
    route = mr.resolve_codex_route("tidy")
    assert route is not None
    assert route.provider == "zai-openai" and route.model == "glm-5.2"
    # The stamp rides with the codex lane too (x-c703): without it a routed
    # codex worker resolves provider "unknown" and ignores its subagent budget.
    assert route.env == {
        "OPENAI_API_KEY": "oai-key",
        "FNO_ROUTE_PROVIDER": "zai-openai",
    }
    joined = " ".join(route.config_args)
    assert route.config_args[0] == "-c"
    assert "model_providers.zai-openai=" in joined
    assert "base_url = 'https://api.z.ai/api/coding/paas/v4'" in joined
    assert "env_key = 'OPENAI_API_KEY'" in joined
    assert "wire_api = 'chat'" in joined  # default for a third-party endpoint
    assert "model_provider='zai-openai'" in joined
    assert "model='glm-5.2'" in joined

    # A configured wire_api passes through.
    _pin_codex_config(
        tmp_path,
        monkeypatch,
        "zai-openai",
        'protocol = "openai"\n'
        'base_url = "https://api.z.ai/api/coding/paas/v4"\n'
        'api_key_env = "OPENAI_API_KEY"\n'
        'wire_api = "responses"\n',
    )
    monkeypatch.setenv("OPENAI_API_KEY", "k")
    route = mr.resolve_codex_route("tidy")
    assert route is not None
    assert "wire_api = 'responses'" in " ".join(route.config_args)


def test_codex_lane_cross_lane_and_safety_rows(tmp_path, monkeypatch) -> None:
    # The default zai provider is anthropic-protocol -> the codex lane returns
    # None; the claude lane skips an openai provider in the other direction.
    assert mr.resolve_codex_route("tidy", settings=_settings()) is None
    notes, sink = _collector()
    assert mr.resolve_route(
        "tidy", settings=_openai_settings(), env={"OPENAI_API_KEY": "k"}, notice=sink
    ) is None
    assert any("protocol" in n for n in notes)

    for role in ("implement", "review-verdict"):
        s = _settings(
            providers={
                "oai": {
                    "protocol": "openai",
                    "base_url": "https://x/v4",
                    "api_key_env": "OPENAI_API_KEY",
                }
            },
            roles={role: "oai,glm-5.2"},
        )
        assert mr.resolve_codex_route(role, settings=s) is None, role

    # No key: the lane notices, never silently Anthropic-bills.
    _pin_codex_config(
        tmp_path,
        monkeypatch,
        "zai-openai",
        'protocol = "openai"\n'
        'base_url = "https://api.z.ai/api/coding/paas/v4"\n'
        'api_key_env = "OPENAI_API_KEY"\n',
    )
    monkeypatch.delenv("OPENAI_API_KEY", raising=False)
    notes, sink = _collector()
    assert mr.resolve_codex_route("tidy", notice=sink) is None
    assert notes


@requires_rust
def test_codex_route_bails_on_unsafe_provider_name(tmp_path, monkeypatch) -> None:
    notes, sink = _collector()
    # A dot is a valid single non-whitespace token but not a safe codex bareword
    # provider id, so the Rust builder's own guard bails + notices.
    _pin_codex_config(
        tmp_path,
        monkeypatch,
        "b.ad",
        'protocol = "openai"\n'
        'base_url = "https://x/v4"\n'
        'api_key_env = "OPENAI_API_KEY"\n',
    )
    monkeypatch.setenv("OPENAI_API_KEY", "k")
    assert mr.resolve_codex_route("tidy", notice=sink) is None
    assert any("safe codex provider id" in n for n in notes), notes


@requires_rust
def test_codex_route_bails_on_unquotable_value(tmp_path, monkeypatch) -> None:
    # A single quote OR any control char (incl. NUL) can't be embedded -> bail.
    notes, sink = _collector()
    import json as _json

    cfg = tmp_path / "config.toml"
    cfg.write_text(
        "[model_routing]\n"
        '[model_routing.roles]\ntidy = "oai,glm-5.2"\n'
        "[model_routing.providers.oai]\n"
        "protocol = \"openai\"\nbase_url = "
        + _json.dumps("https://x/v4'inject")
        + "\n"
        'api_key_env = "OPENAI_API_KEY"\n',
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    monkeypatch.setenv("OPENAI_API_KEY", "k")
    assert mr.resolve_codex_route("tidy", notice=sink) is None


# --- the [1m] compact window ------------------------------------------------------------------

def test_compact_window_rows() -> None:
    # 800000, not 1000000: the [1m] variant already selects 1M context, so a 1M
    # threshold is a no-op; 800000 is the ~80% backstop above the king nudge.
    route = mr.resolve_route(
        "tidy",
        settings=_settings(roles={"tidy": "zai,glm-5.2[1m]"}),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is not None
    assert route["CLAUDE_CODE_AUTO_COMPACT_WINDOW"] == "800000"
    route = mr.resolve_route("consolidate", settings=_settings(), env={"ZAI_API_KEY": "k"})
    assert route is not None
    assert "CLAUDE_CODE_AUTO_COMPACT_WINDOW" not in route
    route = mr.resolve_route(
        "tidy",
        settings=_settings(
            roles={"tidy": "zai,glm-5.2[1m]"},
            extra_env={"CLAUDE_CODE_AUTO_COMPACT_WINDOW": "500000"},
        ),
        env={"ZAI_API_KEY": "k"},
    )
    assert route is not None
    assert route["CLAUDE_CODE_AUTO_COMPACT_WINDOW"] == "500000"


# --- build lane ----------------------------------------------------------------------------------

def test_build_lane_rows() -> None:
    # build is NOT auto-routed; writing model_routing.roles.build is the opt-in.
    assert "build" not in mr.DEFAULT_ROUTED_ROLES
    assert mr.resolve_route("build", settings=_settings(), env={"ZAI_API_KEY": "k"}) is None
    route = mr.resolve_route(
        "build",
        settings=_settings(roles={"build": "zai,glm-5.2"}),
        env={"ZAI_API_KEY": "zk"},
    )
    assert route is not None
    assert route["ANTHROPIC_BASE_URL"] == "https://api.z.ai/api/anthropic"
    assert route["ANTHROPIC_MODEL"] == "glm-5.2"
    notes, sink = _collector()
    assert mr.resolve_route(
        "build", settings=_settings(roles={"build": "zai,glm-5.2"}), env={}, notice=sink
    ) is None
    assert any("ZAI_API_KEY" in n or "no API key" in n for n in notes)
    assert "build" in mr.KNOWN_LANE_ROLES
    assert "build" not in mr.PROTECTED_ROLES
    route = mr.resolve_route(
        "build",
        settings=_settings(roles={"build": "zai/glm-5.2"}),
        env={"ZAI_API_KEY": "zk"},
    )
    assert route is not None and route["ANTHROPIC_MODEL"] == "glm-5.2"


def test_parse_target_rows() -> None:
    for raw in ("zai,glm 5.2", "zai,glm\n5.2", "z ai,glm-5.2", "zai,gl\tm"):
        assert mr._parse_target(raw) is None, raw
    assert mr._parse_target("zai,glm-5.2[1m]") == ("zai", "glm-5.2[1m]")
    rows = [
        ("zai/glm-5.2", ("zai", "glm-5.2")),
        ("zai/glm-5.2[1m]", ("zai", "glm-5.2[1m]")),
        ("zai-openai/glm-4.6", ("zai-openai", "glm-4.6")),
        # First slash splits, so a namespaced model id keeps its slashes.
        ("zai/z-ai/glm-5.2", ("zai", "z-ai/glm-5.2")),
        ("zai,glm-5.2", ("zai", "glm-5.2")),
    ]
    for raw, expected in rows:
        assert mr._parse_target(raw) == expected, raw


# --- x-5cc5: a recorded overlay's provider-DEFAULT tier re-resolves on resume -----

def _recorded_route(
    haiku: str = "glm-4.5-air", base: str = mr.DEFAULT_ZAI_BASE_URL
) -> dict[str, str]:
    return {
        "ANTHROPIC_BASE_URL": base,
        "ANTHROPIC_AUTH_TOKEN": "zk-secret",
        "ANTHROPIC_MODEL": "glm-5.3",
        "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.3",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL": haiku,
    }


def test_refresh_rows() -> None:
    # A file recorded when glm-4.5-air was the zai default keeps failing after
    # the default moved; the refresh lands on today's default, names old/new,
    # replays operator keys verbatim, and never mutates the caller's mapping.
    route = _recorded_route()
    refreshed, note = mr.refresh_provider_default_tiers(route, settings=_settings())
    assert refreshed["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "glm-4.7"
    assert note is not None
    assert "zai" in note and "glm-4.5-air" in note and "glm-4.7" in note
    assert refreshed["ANTHROPIC_AUTH_TOKEN"] == "zk-secret"
    assert refreshed["ANTHROPIC_MODEL"] == "glm-5.3"
    assert route["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "glm-4.5-air"

    route = _recorded_route(haiku="glm-4.7")
    refreshed, note = mr.refresh_provider_default_tiers(route, settings=_settings())
    assert note is None and refreshed == route

    # An unknown base_url replays unchanged; the disclosure names the miss.
    route = _recorded_route(base="https://elsewhere.example/api")
    refreshed, note = mr.refresh_provider_default_tiers(route, settings=_settings())
    assert refreshed == route
    assert note is not None and "https://elsewhere.example/api" in note

    # A mirrored base_url still resolves through the provider stamp.
    route = _recorded_route(base="https://mirror.example/api")
    route[mr.ROUTE_PROVIDER_ENV] = "zai"
    refreshed, note = mr.refresh_provider_default_tiers(route, settings=_settings())
    assert refreshed["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "glm-4.7"
    assert note is not None and "zai" in note

    # A config-pinned haiku_model is a NAMED default.
    route = _recorded_route()
    settings = _settings(providers={"zai": {"haiku_model": "glm-4.6"}})
    refreshed, note = mr.refresh_provider_default_tiers(route, settings=settings)
    assert refreshed["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "glm-4.6"
    assert note is not None and "glm-4.6" in note

    # AC4-HP: a tier declared between record and resume re-resolves too.
    route = _recorded_route(haiku="glm-4.7")
    route["ANTHROPIC_DEFAULT_OPUS_MODEL"] = "glm-5.2"
    route["ANTHROPIC_DEFAULT_SONNET_MODEL"] = "glm-5.3"
    settings = _settings(providers={"zai": {"tier_models": {"opus": "glm-5.3[1m]"}}})
    refreshed, note = mr.refresh_provider_default_tiers(route, settings=settings)
    assert refreshed["ANTHROPIC_DEFAULT_OPUS_MODEL"] == "glm-5.3[1m]"
    assert refreshed["ANTHROPIC_DEFAULT_SONNET_MODEL"] == "glm-5.3"
    assert refreshed["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "glm-4.7"
    assert note is not None and "opus glm-5.2 -> glm-5.3[1m]" in note

    assert mr.provider_name_for_route({}, settings=_settings()) is None
    assert (
        mr.provider_name_for_route(
            {"ANTHROPIC_BASE_URL": "https://nope.example"}, settings=_settings()
        )
        is None
    )
