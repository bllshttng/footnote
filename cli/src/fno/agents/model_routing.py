"""Role-based per-spawn model routing for fno agents (x-d2fe).

Auxiliary coordination work (backlog tidying, node orientation, memory
consolidation) is routed to a *secondary* model provider (z.ai GLM, DeepSeek,
...) via the worker's environment at spawn time; production work (writing the
diff, the correctness verdict) stays on the primary Anthropic model,
byte-for-byte as today.

The ``build`` lane extends the same mechanism to *delivery* spawns (``/target
bg`` + blueprint autolaunch). It is opt-in by config presence: unconfigured it
routes nothing (fail-safe ``None``); writing ``model_routing.roles.build`` is
the consent. ``build`` is not in :data:`DEFAULT_ROUTED_ROLES` but is named in
:data:`KNOWN_LANE_ROLES` so ``fno route ls`` renders it even before it is set.

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

# Routable lanes that are part of the known vocabulary but are NOT auto-routed
# by default: config presence is the opt-in (writing model_routing.roles.build
# IS the consent). `_role_target` already resolves any config-present name, so
# these need no resolution-path change; they exist so `fno route ls` can render
# a lane that has no config line yet (an unconfigured `build` row) instead of
# hiding it. `build` is the sanctioned delivery lane for /target bg + blueprint
# autolaunch; unconfigured it fails safe to the primary Anthropic model.
KNOWN_LANE_ROLES = ("build",)

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
    """Parse a ``"provider/model"`` (or legacy ``"provider,model"``) target into
    (provider, model).

    Slash is the canonical, ecosystem-standard form (``zai/glm-5.2[1m]``); comma
    is still accepted for the existing peer-lane / built-in values and any config
    written before the switch, so nothing already configured breaks. A comma, if
    present, wins as the separator (it never appears inside a model id, so it is
    unambiguous); otherwise the FIRST slash splits, which keeps a namespaced model
    id intact (``zai/z-ai/glm-5.2`` -> provider ``zai``, model ``z-ai/glm-5.2``).

    Returns None for a malformed value (fail-safe; caller degrades to primary)."""
    raw = raw.strip()
    if "," in raw:
        parts = [p.strip() for p in raw.split(",")]
        if len(parts) != 2:
            return None
        provider, model = parts
    else:
        provider, found, model = raw.partition("/")
        if not found:
            return None
        provider, model = provider.strip(), model.strip()
    if not provider or not model:
        return None
    # Reject INTERNAL whitespace (a space or newline inside a token): the
    # contract is one non-whitespace model token, an embedded space yields an
    # invalid model id and a newline would corrupt the line-oriented dispatch
    # receipt. Outer whitespace was already stripped above.
    if any(c.isspace() for c in provider) or any(c.isspace() for c in model):
        return None
    return provider.lower(), model


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


def key_source(
    provider: Mapping[str, Optional[str]], env: Optional[Mapping[str, str]] = None
) -> tuple[Optional[str], list[str]]:
    """Report WHERE a provider's key resolved, never the key value itself.

    Returns ``(satisfying, checked)``: ``satisfying`` is the env-var name or the
    file path that yielded a key (``None`` when missing); ``checked`` lists every
    source consulted, in precedence order, for a legible ``MISSING (checked ...)``
    message. Same env-beats-file precedence as :func:`_resolve_key`; used by
    ``fno route ls`` so the key column names its source without leaking secrets.
    """
    if env is None:
        import os

        env = os.environ
    checked: list[str] = []
    key_name = provider.get("api_key_env") or ""
    if key_name:
        checked.append(key_name)
        if env.get(key_name):
            return key_name, checked
    key_file = provider.get("api_key_file")
    if key_file and key_name:
        checked.append(str(key_file))
        if _key_from_env_file(str(key_file), key_name):
            return str(key_file), checked
    return None, checked


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
    return _route_for_target(pname, model, block, env, notice, ctx=f"role {name!r}")


def _route_for_target(
    pname: str,
    model: str,
    block: "ModelRoutingBlock",
    env: Mapping[str, str],
    notice: Optional[Callable[[str], object]],
    *,
    ctx: str,
) -> Optional[dict[str, str]]:
    """Build the routed ``ANTHROPIC_*`` env for an explicit ``(provider, model)``.

    Shared by ``resolve_route`` (role -> target) and ``resolve_explicit_route``
    (peer lane names the target directly). Returns None (fail-safe, never raises)
    when the provider is unknown/non-anthropic/keyless. ``ctx`` names the caller
    ("role 'consolidate'" / "peer 'zai'") for the fail-safe notices."""
    # A role that can't route falls back to the primary Anthropic model; a PEER
    # that can't route is SKIPPED entirely (there is no author-model fallback for
    # a peer - that would be the same model as the author). Word the fail-safe
    # notice for the actual outcome so the operator is not told a skipped GLM peer
    # "fell back to the primary model" when nothing ran.
    fallback = "skipping the peer" if ctx.startswith("peer") else "using the primary model"
    fallback_key = (
        "skipping the peer"
        if ctx.startswith("peer")
        else "falling back to the primary Anthropic model"
    )
    provider = _resolve_provider(pname, block)
    if provider is None:
        _emit(
            notice,
            f"model-routing: provider {pname!r} for {ctx} is not "
            f"configured; {fallback}",
        )
        return None

    protocol = (provider.get("protocol") or "anthropic").lower()
    if protocol != "anthropic":
        _emit(
            notice,
            f"model-routing: provider {pname!r} uses the {protocol!r} protocol, "
            f"which a claude worker cannot use; {fallback}",
        )
        return None

    base_url = provider.get("base_url") or ""
    if not base_url:
        _emit(
            notice,
            f"model-routing: provider {pname!r} has no base_url; {fallback}",
        )
        return None

    key = _resolve_key(provider, env)
    if not key:
        _emit(
            notice,
            f"model-routing: no API key for provider {pname!r} ({ctx}); "
            f"{fallback_key}",
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


def resolve_explicit_route(
    provider: str,
    model: str,
    *,
    settings: "Optional[SettingsModel]" = None,
    env: Optional[Mapping[str, str]] = None,
    notice: Optional[Callable[[str], object]] = None,
) -> Optional[dict[str, str]]:
    """Resolve the routed ``ANTHROPIC_*`` env for an explicit ``provider,model``.

    The peer lane (``config.review.peers`` entry ``{provider: claude, model:
    "zai,glm-5.2"}``) names its route directly rather than via a role, so it
    bypasses both the role->target lookup AND the ``PROTECTED_ROLES`` / global
    ``enabled`` role-auto-routing policy (an explicit peer opt-in is not
    auto-routing). It reuses the SAME provider-registry key/env logic as
    ``resolve_route`` so the z.ai env-var contract lives in exactly one place.

    Returns None (fail-safe, never raises) when the provider is unknown,
    non-anthropic, or keyless - the caller (peer skill) then skips the GLM peer
    rather than silently falling back to the Anthropic-billed author model.
    """
    pname = _normalize(provider)
    if not pname or not (model or "").strip():
        return None
    if env is None:
        import os

        env = os.environ
    block = _routing_block(settings)
    return _route_for_target(
        pname, model.strip(), block, env, notice, ctx=f"peer {pname!r}"
    )


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
    """Wrap ``value`` as a TOML literal string (single-quoted, no escapes), or
    None if it can't be embedded safely. Rejects a single quote (would break the
    literal string) AND any control char - notably NUL, which would otherwise
    survive into the codex argv and make ``subprocess`` raise ``ValueError:
    embedded null byte``, breaking resolve_codex_route's never-raise contract.
    These are controlled config values we won't try to escape; bail fail-safe."""
    if "'" in value or any(ord(c) < 0x20 for c in value):
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
    return settings.model_routing


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


def effective_providers(
    block: "ModelRoutingBlock",
) -> dict[str, dict[str, Optional[str]]]:
    """Public: the merged built-in + config providers map (config wins per field).

    Used by ``fno route ls`` (render) and ``fno route set`` (reject a target
    naming a provider absent from this map) so provider knowledge lives in one
    place."""
    providers: dict[str, dict[str, Optional[str]]] = {
        name: dict(rec) for name, rec in _DEFAULT_PROVIDERS.items()
    }
    for name, rec in (getattr(block, "providers", None) or {}).items():
        merged = dict(providers.get(name, {}))
        merged.update(_provider_to_dict(rec))
        providers[name] = merged
    return providers


# Static provenance: which auto-dispatch surface passes each role. Everything
# not named here is "manual only" (a role reachable only by an explicit --role).
AUTO_ASSIGNED_BY: dict[str, str] = {
    "post-merge": "pr_watch post-merge dispatch",
    "codex-verify": "codex verify lane",
    "build": "/target bg + blueprint autolaunch",
}


def build_route_table(
    *,
    settings: "Optional[SettingsModel]" = None,
    env: Optional[Mapping[str, str]] = None,
) -> list[dict[str, str]]:
    """The effective routing table, one row per role, for ``fno route ls``.

    Merges built-in roles/providers with config overrides. Each row is the
    canonical 5-field shape: ``role | target | protocol | key | assigned_by``
    (AC1-UI). Protected roles render as explicit never-routed rows; a known lane
    with no config line (``build``) renders as unconfigured. Degrades a missing
    key to ``MISSING (checked ...)`` rather than raising, so an unreadable/absent
    ``.env`` never errors the whole table (AC-degraded). Never raises.
    """
    if env is None:
        import os

        env = os.environ
    block = _routing_block(settings)
    eff = _effective_roles(block)
    config_roles = {
        str(k).strip().lower() for k in (getattr(block, "roles", None) or {})
    }

    names: list[str] = []
    for r in (*DEFAULT_ROUTED_ROLES, *KNOWN_LANE_ROLES, *sorted(config_roles), *sorted(PROTECTED_ROLES)):
        if r not in names:
            names.append(r)

    def _provenance(role: str, source: str) -> str:
        return f"{AUTO_ASSIGNED_BY.get(role, 'manual only')} ({source})"

    rows: list[dict[str, str]] = []
    for role in names:
        if role in PROTECTED_ROLES:
            rows.append(
                {
                    "role": role,
                    "target": "never routed (hard guard)",
                    "protocol": "-",
                    "key": "-",
                    "assigned_by": "protected (hard guard)",
                }
            )
            continue
        source = "config" if role in config_roles else (
            "built-in" if role in DEFAULT_ROUTED_ROLES else "known lane"
        )
        raw = eff.get(role)
        if not raw:
            rows.append(
                {
                    "role": role,
                    "target": "unconfigured",
                    "protocol": "-",
                    "key": "-",
                    "assigned_by": _provenance(role, "unconfigured"),
                }
            )
            continue
        parsed = _parse_target(str(raw))
        if parsed is None:
            rows.append(
                {
                    "role": role,
                    "target": f"{raw} (malformed)",
                    "protocol": "-",
                    "key": "-",
                    "assigned_by": _provenance(role, source),
                }
            )
            continue
        pname, model = parsed
        provider = _resolve_provider(pname, block)
        if provider is None:
            rows.append(
                {
                    "role": role,
                    "target": f"{pname}/{model}",
                    "protocol": "unknown provider",
                    "key": "-",
                    "assigned_by": _provenance(role, source),
                }
            )
            continue
        protocol = (provider.get("protocol") or "anthropic").lower()
        satisfying, checked = key_source(provider, env)
        key_status = (
            f"found via {satisfying}"
            if satisfying
            else f"MISSING (checked {', '.join(checked) or 'nothing'})"
        )
        rows.append(
            {
                "role": role,
                "target": f"{pname}/{model}",
                "protocol": protocol,
                "key": key_status,
                "assigned_by": _provenance(role, source),
            }
        )
    return rows
