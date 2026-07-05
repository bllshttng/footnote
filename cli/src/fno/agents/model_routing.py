"""Role-based per-spawn model routing for fno agents (x-d2fe).

Auxiliary coordination work (backlog tidying, node orientation, memory
consolidation) is routed to a *secondary* model provider (z.ai GLM, DeepSeek,
...) via the worker's environment at spawn time; production work (writing the
diff, the correctness verdict) stays on the primary Anthropic model,
byte-for-byte as today.

Mechanism (Locked Decision 2): a spawn stamps ``ANTHROPIC_BASE_URL`` +
``ANTHROPIC_AUTH_TOKEN`` + the model env vars into the worker env. No proxy in
the critical path; each worker is a fresh process, so switching base_url per
spawn is safe (never switch base_url mid-session).

A spawned worker is ``claude --bg``, which speaks the **Anthropic** Messages
API. So a provider is usable here only if it exposes an Anthropic-compatible
endpoint (z.ai: ``https://api.z.ai/api/anthropic``; DeepSeek:
``https://api.deepseek.com/anthropic``). The OpenAI-protocol endpoints those
same vendors publish (z.ai's ``/api/coding/paas/v4``) are for OpenAI-SDK
consumers and a future codex/openai lane, not for a claude worker. A provider
whose ``protocol`` is not ``anthropic`` is skipped here with a notice.

Claude Code internally requests opus/sonnet/haiku tiers (background tasks use
haiku). Setting ``ANTHROPIC_MODEL`` + the three ``ANTHROPIC_DEFAULT_*`` tier
vars to the routed model sends the WHOLE worker to the secondary provider, so no
Anthropic usage is recorded (AC1-HP). The background (haiku) tier defaults to
the provider's cheaper ``haiku_model`` (zai -> ``glm-4.5-air``) so judgment-light
background traffic runs cheap on the SAME secondary provider; opus/sonnet stay
on the role model. A provider with no ``haiku_model`` keeps the role model on
every tier. Operators can further differentiate any tier via ``extra_env``.

A routed model name carrying the ``[1m]`` suffix (1M-context) also gets
``CLAUDE_CODE_AUTO_COMPACT_WINDOW=1000000`` injected, or the 1M window is
silently lost; an explicit ``extra_env`` value wins.

Two non-negotiable invariants:

- **Fail safe, not fail closed** (US4): if no key is configured for the role's
  provider, the role falls back to the primary Anthropic model with a one-line
  notice and the spawn still succeeds. :func:`resolve_route` never raises.
- **Hard quality guard**: ``implement`` / ``review-verdict`` are in
  ``PROTECTED_ROLES`` and short-circuit to ``None`` *before* any config is read.
  No settings edit can route the diff or verdict to a secondary provider.
"""
from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING, Callable, Mapping, NamedTuple, Optional

if TYPE_CHECKING:
    from fno.config import ModelRoutingBlock, SettingsModel

# Built-in default endpoint for the z.ai provider: the Anthropic-compatible
# endpoint a claude worker needs (NOT the OpenAI /api/coding/paas/v4 path).
DEFAULT_ZAI_BASE_URL = "https://api.z.ai/api/anthropic"

# Default secondary model for routed roles. glm-4.5-air is too weak for
# reasoning-bearing work; a current flagship GLM does real work. Pin a cheaper
# model per role via the roles map. Kept in lockstep with the schema default
# (drift-guarded by test_config_defaults_match_module_constants).
DEFAULT_SECONDARY_MODEL = "glm-5.2"

# Cheaper model for the background (haiku) tier of the built-in zai provider.
# Claude Code runs background tasks on haiku; routing the haiku tier to this
# cheaper GLM keeps judgment-light background traffic cheap while opus/sonnet
# stay on the role model. Kept in lockstep with the schema default
# (drift-guarded by test_config_defaults_match_module_constants).
DEFAULT_ZAI_HAIKU_MODEL = "glm-4.5-air"

# Built-in providers so a bare key (e.g. ZAI_API_KEY) routes with zero config.
# A config.model_routing.providers entry of the same name overrides per-field.
_DEFAULT_PROVIDERS: dict[str, dict[str, Optional[str]]] = {
    "zai": {
        "protocol": "anthropic",
        "base_url": DEFAULT_ZAI_BASE_URL,
        "api_key_env": "ZAI_API_KEY",
        "api_key_file": None,
        "haiku_model": DEFAULT_ZAI_HAIKU_MODEL,
    },
}

# Roles routed to a secondary provider by default (provider 'zai', the default
# model). The config roles map overrides per role. ``post-merge`` is the
# recurring, judgment-light post-merge ritual fire (reconcile / retro / triage);
# its dangerous state mutations stay deterministic CLI, so a weaker model only
# writes prose + picks triage. Routable, NOT protected (fail-safe to Anthropic
# without a key, like every routed role).
DEFAULT_ROUTED_ROLES = ("coordinate", "tidy", "orient", "consolidate", "post-merge")

# Roles the secondary provider must NEVER touch (writes a diff / renders a
# correctness verdict). Hard guard, enforced before any config is read.
PROTECTED_ROLES = frozenset({"implement", "review-verdict"})

# Every tier Claude Code may request internally. Setting all of them to the
# routed model keeps the entire worker (incl. background haiku) on the secondary
# provider, so zero Anthropic usage is recorded.
_MODEL_ENV_KEYS = (
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
)


def _emit(notice: Optional[Callable[[str], object]], message: str) -> None:
    """Surface a one-line fail-safe notice; quiet when no sink is supplied."""
    if notice is not None:
        notice(message)


def _normalize(role: Optional[str]) -> str:
    return (role or "").strip().lower()


def _parse_target(raw: str) -> Optional[tuple[str, str]]:
    """Parse a ``"provider,model"`` role value into (provider, model).

    Returns None for a malformed value (fail-safe; caller degrades to primary)."""
    parts = [p.strip() for p in raw.split(",")]
    if len(parts) != 2 or not parts[0] or not parts[1]:
        return None
    return parts[0].lower(), parts[1]


def _key_from_env_file(path_str: str, key_name: str) -> Optional[str]:
    """Read ``key_name`` from a ``.env``-style file. Missing file / key is not
    fatal: returns None so the caller falls back to the primary model.

    Tolerates an optional ``export`` prefix and whitespace around ``=``.
    RuntimeError is caught alongside OSError/ValueError because
    ``Path.expanduser()`` raises it when the home dir cannot be resolved."""
    try:
        text = Path(path_str).expanduser().read_text(encoding="utf-8")
    except (OSError, ValueError, RuntimeError):
        return None
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("#") or "=" not in line:
            continue
        if line.startswith("export "):
            line = line[len("export "):].strip()
        name, _, value = line.partition("=")
        if name.strip() == key_name:
            return value.strip().strip('"').strip("'") or None
    return None


def _resolve_key(
    provider: Mapping[str, Optional[str]], env: Mapping[str, str]
) -> Optional[str]:
    """Resolve a provider's API key. Precedence: process env (named by
    ``api_key_env``) wins over the optional ``api_key_file``. Never raises."""
    key_name = provider.get("api_key_env") or ""
    if key_name:
        from_env = env.get(key_name)
        if from_env:
            return from_env
    key_file = provider.get("api_key_file")
    if key_file and key_name:
        return _key_from_env_file(key_file, key_name)
    return None


def resolve_route(
    role: Optional[str],
    *,
    settings: "Optional[SettingsModel]" = None,
    env: Optional[Mapping[str, str]] = None,
    notice: Optional[Callable[[str], object]] = None,
) -> Optional[dict[str, str]]:
    """Resolve per-spawn env overrides for ``role``.

    Returns a dict of env keys to merge into the worker's spawn env, or ``None``
    meaning "use the primary Anthropic model, change nothing".

    ``None`` is returned for: no role, a production/unrouted role, a disabled
    block, an unconfigured / non-Anthropic provider, or a missing key
    (fail-safe). This function never raises.

    Args:
        role: the spawn's role; case/space-insensitive.
        settings: a SettingsModel; loaded from config when None.
        env: environment mapping for key lookup; ``os.environ`` when None.
        notice: optional one-line-notice sink for fail-safe fallbacks.
    """
    name = _normalize(role)
    if not name:
        return None

    # Hard quality guard FIRST: a protected role never routes, even if config
    # tries to. Short-circuits before reading config.
    if name in PROTECTED_ROLES:
        return None

    if env is None:
        import os

        env = os.environ

    block = _routing_block(settings)
    if not getattr(block, "enabled", True):
        return None

    target = _role_target(name, block)
    if target is None:
        return None  # not a routed role -> primary model
    pname, model = target

    provider = _resolve_provider(pname, block)
    if provider is None:
        _emit(
            notice,
            f"model-routing: provider {pname!r} for role {name!r} is not "
            f"configured; using the primary model",
        )
        return None

    protocol = (provider.get("protocol") or "anthropic").lower()
    if protocol != "anthropic":
        _emit(
            notice,
            f"model-routing: provider {pname!r} uses the {protocol!r} protocol, "
            f"which a claude worker cannot use; using the primary model",
        )
        return None

    base_url = provider.get("base_url") or ""
    if not base_url:
        _emit(
            notice,
            f"model-routing: provider {pname!r} has no base_url; "
            f"using the primary model",
        )
        return None

    key = _resolve_key(provider, env)
    if not key:
        _emit(
            notice,
            f"model-routing: no API key for provider {pname!r} (role {name!r}); "
            f"falling back to the primary Anthropic model",
        )
        return None

    route = {"ANTHROPIC_BASE_URL": base_url, "ANTHROPIC_AUTH_TOKEN": key}
    for k in _MODEL_ENV_KEYS:
        route[k] = model
    # Item 1: route the background (haiku) tier to the provider's cheaper
    # haiku_model (zai -> glm-4.5-air). Still the SAME secondary provider
    # (base_url + token), so the whole worker stays off Anthropic; only the
    # background model is cheaper. A provider with no haiku_model keeps the role
    # model on the haiku tier (no regression, never an empty/invalid id).
    haiku_model = provider.get("haiku_model")
    if haiku_model:
        route["ANTHROPIC_DEFAULT_HAIKU_MODEL"] = haiku_model
    # Item 2: a routed model carrying the [1m] suffix needs the 1M-context
    # compact window or it silently loses the window. Injected before extra_env
    # so an explicit extra_env value still wins.
    if model.endswith("[1m]"):
        route["CLAUDE_CODE_AUTO_COMPACT_WINDOW"] = "1000000"
    # extra_env (timeouts, flags, per-tier model overrides) merged last so an
    # operator can differentiate tiers or tune the routed worker.
    for k, v in (getattr(block, "extra_env", None) or {}).items():
        route[str(k)] = str(v)
    return route


# Default codex wire protocol for a third-party OpenAI-compatible endpoint
# (z.ai's paas/v4 speaks Chat Completions). Codex's own default is "responses"
# (OpenAI's API); a routed third-party provider almost always wants "chat".
DEFAULT_CODEX_WIRE_API = "chat"


class CodexRoute(NamedTuple):
    """Codex-lane (OpenAI-protocol) routing result.

    Codex does NOT read ``OPENAI_BASE_URL`` from the env; a custom endpoint is
    selected via inline ``-c`` config (``model_providers.<name>=...`` +
    ``model_provider=<name>`` + ``model=<model>``), with the API key supplied in
    the ``env_key`` env var the config names. ``env`` is merged into the codex
    spawn env; ``config_args`` is prepended to the codex argv as global flags.
    """

    env: dict[str, str]
    config_args: list[str]


def _toml_literal(value: str) -> Optional[str]:
    """Wrap ``value`` as a TOML literal string (single-quoted, no escapes). A
    value containing a single quote or newline can't be a literal string and is
    controlled config we won't try to escape; return None so the caller bails
    fail-safe rather than emitting malformed TOML into the codex argv."""
    if "'" in value or "\n" in value or "\r" in value:
        return None
    return f"'{value}'"


def resolve_codex_route(
    role: Optional[str],
    *,
    settings: "Optional[SettingsModel]" = None,
    env: Optional[Mapping[str, str]] = None,
    notice: Optional[Callable[[str], object]] = None,
) -> Optional[CodexRoute]:
    """Resolve codex-lane routing for ``role`` against an OpenAI-protocol
    provider, or ``None`` (use codex's default config, change nothing).

    Mirrors :func:`resolve_route`'s fail-safe contract: a protected role, a
    disabled block, an unrouted role, an unconfigured provider, a NON-openai
    provider (that belongs to the claude lane), a missing base_url/key, or a
    value that can't be safely embedded in TOML all return ``None`` (with a
    one-line notice where a misconfiguration is worth surfacing). Never raises.
    """
    name = _normalize(role)
    if not name or name in PROTECTED_ROLES:
        return None

    if env is None:
        import os

        env = os.environ

    block = _routing_block(settings)
    if not getattr(block, "enabled", True):
        return None

    target = _role_target(name, block)
    if target is None:
        return None
    pname, model = target

    provider = _resolve_provider(pname, block)
    if provider is None:
        _emit(
            notice,
            f"model-routing (codex): provider {pname!r} for role {name!r} is "
            f"not configured; using the default codex model",
        )
        return None

    protocol = (provider.get("protocol") or "anthropic").lower()
    if protocol != "openai":
        # An anthropic provider belongs to the claude lane, not here. Silent
        # None (not an error): a role shared across lanes just no-ops on codex.
        return None

    base_url = provider.get("base_url") or ""
    if not base_url:
        _emit(notice, f"model-routing (codex): provider {pname!r} has no base_url")
        return None

    api_key_env = provider.get("api_key_env") or "OPENAI_API_KEY"
    key = _resolve_key(provider, env)
    if not key:
        _emit(
            notice,
            f"model-routing (codex): no API key for provider {pname!r} "
            f"(role {name!r}); using the default codex model",
        )
        return None

    # The provider name becomes a TOML table key (model_providers.<pname>) and a
    # config value; only a bareword identifier is safe in the -c argument.
    import re

    if not re.fullmatch(r"[A-Za-z0-9_-]+", pname):
        _emit(
            notice,
            f"model-routing (codex): provider name {pname!r} is not a safe codex "
            f"provider id; using the default codex model",
        )
        return None

    wire_api = provider.get("wire_api") or DEFAULT_CODEX_WIRE_API

    # Build the inline model-provider config. Every embedded value is a TOML
    # literal string; a value we can't embed safely aborts the route.
    lits = {
        "base_url": _toml_literal(base_url),
        "env_key": _toml_literal(api_key_env),
        "wire_api": _toml_literal(wire_api),
        "provider": _toml_literal(pname),
        "model": _toml_literal(model),
    }
    if any(v is None for v in lits.values()):
        _emit(
            notice,
            f"model-routing (codex): provider {pname!r} has a value that can't "
            f"be embedded in TOML; using the default codex model",
        )
        return None

    provider_table = (
        f"model_providers.{pname}={{ base_url = {lits['base_url']}, "
        f"env_key = {lits['env_key']}, wire_api = {lits['wire_api']} }}"
    )
    config_args = [
        "-c",
        provider_table,
        "-c",
        f"model_provider={lits['provider']}",
        "-c",
        f"model={lits['model']}",
    ]
    return CodexRoute(env={api_key_env: key}, config_args=config_args)


def _routing_block(settings: "Optional[SettingsModel]") -> "ModelRoutingBlock":
    """Return the model_routing config block, loading settings if needed."""
    if settings is None:
        from fno.config import load_settings

        settings = load_settings()
    return settings.config.model_routing


def _effective_roles(block: "ModelRoutingBlock") -> dict[str, str]:
    """Built-in routed roles (-> the default provider+model) overlaid with the
    config roles map (per-role override wins)."""
    eff: dict[str, str] = {
        r: f"zai,{DEFAULT_SECONDARY_MODEL}" for r in DEFAULT_ROUTED_ROLES
    }
    for k, v in (getattr(block, "roles", None) or {}).items():
        eff[str(k).strip().lower()] = str(v)
    return eff


def _role_target(role: str, block: "ModelRoutingBlock") -> Optional[tuple[str, str]]:
    """Resolve (provider, model) for a routed ``role``, or None if not routed."""
    raw = _effective_roles(block).get(role)
    if not raw:
        return None
    return _parse_target(str(raw))


def _provider_to_dict(provider: object) -> dict[str, Optional[str]]:
    """Normalize a config ModelProvider (or a plain dict, for tests) to a dict
    of ONLY the explicitly-set fields, so a partial override of a built-in
    provider (e.g. ``zai: {api_key_file: ...}``) keeps the built-in's other
    fields (base_url, api_key_env) instead of clobbering them with empty
    pydantic defaults."""
    if hasattr(provider, "model_dump"):
        return dict(provider.model_dump(exclude_unset=True))
    if isinstance(provider, dict):
        return dict(provider)
    return {}


def _resolve_provider(
    pname: str, block: "ModelRoutingBlock"
) -> Optional[dict[str, Optional[str]]]:
    """Merge built-in providers with config.providers (config wins per field)
    and return the named provider's record, or None if unknown."""
    providers: dict[str, dict[str, Optional[str]]] = {
        name: dict(rec) for name, rec in _DEFAULT_PROVIDERS.items()
    }
    for name, rec in (getattr(block, "providers", None) or {}).items():
        merged = dict(providers.get(name, {}))
        merged.update(_provider_to_dict(rec))
        providers[name] = merged
    return providers.get(pname)
