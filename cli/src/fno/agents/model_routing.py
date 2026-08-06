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

Claude Code internally requests the opus/sonnet/haiku/fable tiers (background
tasks use haiku). Setting ``ANTHROPIC_MODEL`` + EVERY ``ANTHROPIC_DEFAULT_*`` tier
var to the routed model sends the WHOLE worker to the secondary provider, so no
Anthropic usage is recorded (AC1-HP). The background (haiku) tier defaults to
the provider's cheaper ``haiku_model`` (zai -> ``glm-4.5-air``) so judgment-light
background traffic runs cheap on the SAME secondary provider; opus/sonnet stay
on the role model. A provider with no ``haiku_model`` keeps the role model on
every tier. Operators can further differentiate any tier via ``extra_env``.

A routed model name carrying the ``[1m]`` suffix also gets an
``CLAUDE_CODE_AUTO_COMPACT_WINDOW`` backstop injected. The ``[1m]`` variant
already selects the 1M context; this var is the compaction THRESHOLD (how full
the window gets before Claude Code compacts), capped at the model's window, so
it cannot extend anything. It takes precedence over ``/autocompact``,
``--autocompact``, and the saved setting, so the injected value is what the
worker runs with; an explicit ``extra_env`` value still wins.

Two non-negotiable invariants:

- **Fail safe, not fail closed** (US4): if no key is configured for the role's
  provider, the role falls back to the primary Anthropic model with a one-line
  notice and the spawn still succeeds. The legacy :func:`resolve_route` path
  never raises; an opted-in business lookup fails closed on typed resolution
  errors so they cannot be mistaken for legacy fallback.
- **Hard quality guard**: ``implement`` / ``review-verdict`` are in
  ``PROTECTED_ROLES`` and short-circuit to ``None`` *before* any config is read.
  No settings edit can route the diff or verdict to a secondary provider.
"""

from __future__ import annotations

import os
from typing import TYPE_CHECKING, Callable, Mapping, NamedTuple, Optional

from fno.env_file import read_var_from_env_file

if TYPE_CHECKING:
    from fno.config import ModelRoutingBlock, SettingsModel
    from fno.roles import (
        ManifestRoutingResolution,
        ResolvedRole,
        RoleResolution,
        RoleResolutionBlocked,
    )

BusinessRoleLookup = Callable[[str], "RoleResolution | ManifestRoutingResolution"]

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

# Auto-compact threshold injected on [1m]-routed workers. It is the
# compaction THRESHOLD, not a window selector: the [1m] variant already selects
# the 1M context, Claude Code caps the threshold at the model window, and a
# threshold of 1000000 (the old value) is identical to setting nothing, i.e. no
# compaction before the ceiling. The king handoff nudge fires at ~40%
# (target.handoff.king_used_pct_trigger) so the agent prepares (finishes its
# unit, writes the survival brief, decides compact vs handoff); this 80%
# threshold is the BACKSTOP that catches a worker that ignored it, could not
# act, or was mid-loop. 800k leaves ~200k runway for the compaction request
# itself (a bare /compact at the limit fails "Context limit reached").
ONE_M_AUTO_COMPACT_WINDOW = "800000"

# Where a key lands when the operator keeps secrets out of the shell env; the
# built-in fallback keeps env-file keys working for the zero-config zai lane
# (config-defined providers already opt in per-record via api_key_file).
DEFAULT_API_KEY_FILE = "~/.fno/.env"

# Built-in providers so a bare key (e.g. ZAI_API_KEY) routes with zero config.
# A config.model_routing.providers entry of the same name overrides per-field.
_DEFAULT_PROVIDERS: dict[str, dict[str, Optional[str]]] = {
    "zai": {
        "protocol": "anthropic",
        "base_url": DEFAULT_ZAI_BASE_URL,
        "api_key_env": "ZAI_API_KEY",
        "api_key_file": DEFAULT_API_KEY_FILE,
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


class RouteCompositionError(ValueError):
    """A secondary route cannot compose atomically with the active provider."""


class BusinessRoleResolutionBlockedError(RouteCompositionError):
    """A business role exists but its typed resolution failed closed."""

    def __init__(self, result: "RoleResolutionBlocked") -> None:
        self.result = result
        detail = f": {result.detail}" if result.detail else ""
        super().__init__(
            f"business role {result.role.id!r} is blocked ({result.reason.value}){detail}; "
            "no worker launched"
        )


class BusinessRoleRoutingProjectionError(RouteCompositionError):
    """A resolved business role lacks a deterministic provider/model route."""


def resolve_spawn_route(
    role: Optional[str],
    route_env: Optional[Mapping[str, str]] = None,
    *,
    intent: Optional[str] = None,
    notice: Optional[Callable[[str], None]] = None,
    account_overlay: bool = False,
    business_lookup: Optional[BusinessRoleLookup] = None,
) -> Optional[dict[str, str]]:
    """Resolve one spawn route and reject managed-OAuth half-composition.

    This is the single composition decision consumed by CLI and in-process
    pane/bg/headless births. A pre-resolved explicit route wins over ``role``;
    otherwise the normal fail-safe role resolver decides whether routing is
    active. Managed OAuth owns the default Claude credential slot, so applying
    a secondary endpoint/model over that snapshot is refused as one unit -
    unless an account overlay is present AND the route carries its own auth
    (``account_overlay``). The overlay pins CLAUDE_CONFIG_DIR (Keychain OAuth),
    so a route without its own credential would send that OAuth token to the
    route's foreign endpoint; a resolved route is fail-closed and always
    carries ANTHROPIC_AUTH_TOKEN, but a hand-built partial route_env (e.g.
    base-URL-only from a direct dispatch_spawn caller) does not, so the guard
    stays armed for it. With a self-authed route the composition is atomic
    (route wins endpoint+auth+model), and claude prefers an env credential
    over Keychain OAuth, so the split-brain the guard exists to prevent cannot
    recur.
    """
    route = (
        dict(route_env)
        if route_env
        else resolve_route(role, notice=notice, business_lookup=business_lookup)
    )
    if route and os.environ.get("FNO_PROVIDER_AUTH", "").strip().lower() == "managed":
        # An account overlay relaxes the refusal only when the route is
        # self-sufficient (see docstring): without its own auth the route's
        # foreign endpoint would be paired with the account's Keychain OAuth.
        route_self_authed = bool(
            route.get("ANTHROPIC_AUTH_TOKEN") or route.get("ANTHROPIC_API_KEY")
        )
        if not (account_overlay and route_self_authed):
            overlay_id = os.environ.get("FNO_PROVIDER_ID", "").strip() or "unknown"
            route_intent = intent or (
                f"routed role {role!r}" if role is not None else "pre-resolved route"
            )
            raise RouteCompositionError(
                f"refusing {route_intent} over managed OAuth provider {overlay_id!r}: "
                "endpoint, auth, and model must be selected as one provider route; "
                "no worker launched."
            )
    return route


# Routable lanes that are part of the known vocabulary but are NOT auto-routed
# by default: config presence is the opt-in (writing model_routing.roles.build
# IS the consent). `_role_target` already resolves any config-present name, so
# these need no resolution-path change; they exist so `fno route ls` can render
# a lane that has no config line yet (an unconfigured `build` row) instead of
# hiding it. `build` is the sanctioned delivery lane for /target bg + blueprint
# autolaunch; `pr-create` is the PR-creation worker (/pr create). Unconfigured
# either fails safe to the primary Anthropic model - no hardcoded tier.
KNOWN_LANE_ROLES = ("build", "pr-create")

#: Claude tier aliases: the names Claude Code resolves through
#: ``ANTHROPIC_DEFAULT_<TIER>_MODEL``. ``fable`` is one of them and is a live
#: alias here (``fno agents spawn --model fable``); omitting it left the fable
#: tier of a routed worker resolving at Anthropic while every other tier ran on
#: the secondary provider.
TIER_ALIASES = ("opus", "sonnet", "haiku", "fable")

# Every tier Claude Code may request internally. Setting all of them to the
# routed model keeps the entire worker (incl. background haiku) on the secondary
# provider, so zero Anthropic usage is recorded. Derived from TIER_ALIASES so a
# future tier cannot be added in one place and forgotten in the other.
MODEL_ENV_KEYS = (
    "ANTHROPIC_MODEL",
    *(f"ANTHROPIC_DEFAULT_{alias.upper()}_MODEL" for alias in TIER_ALIASES),
)


class TierRemapConflict(RouteCompositionError):
    """A claude spawn names a tier alias the ambient environment redefines.

    ``claude --bg`` resolves the model tier alias against the CALLER's
    environment but resolves its endpoint and credential separately, so a
    worker born under another vendor's remap asks Anthropic for that vendor's
    model id and dies on its first turn behind a ``"status": "live"`` receipt.
    """


def is_anthropic_model(model: str) -> bool:
    """True for an Anthropic model id or tier alias.

    Pinning a tier to a specific Anthropic model
    (``ANTHROPIC_DEFAULT_OPUS_MODEL=claude-opus-4-1``) is a supported Claude
    Code customization, not a vendor conflict: endpoint and model still agree.
    Only a FOREIGN vendor's id (``glm-5.2[1m]``, ``deepseek-...``) creates the
    split this module guards against.
    """
    name = model.strip().lower()
    return name.startswith("claude-") or name in TIER_ALIASES


def tier_remap_conflict(
    model: Optional[str], env: Optional[Mapping[str, str]] = None
) -> Optional[tuple[str, str]]:
    """``(alias, remapped_model)`` when ``model`` is a claude tier alias that
    ``env`` redefines to a FOREIGN vendor's model id, else ``None``. Pure and
    cheap: reads only the env."""
    alias = (model or "").strip().lower()
    if alias not in TIER_ALIASES:
        return None
    if env is None:
        env = os.environ
    remapped = (env.get(f"ANTHROPIC_DEFAULT_{alias.upper()}_MODEL") or "").strip()
    if not remapped or is_anthropic_model(remapped):
        return None
    return alias, remapped


def remap_conflict_message(alias: str, remapped: str) -> str:
    """The one refusal text, shared by the CLI seam and the in-process APIs."""
    var = f"ANTHROPIC_DEFAULT_{alias.upper()}_MODEL"
    return (
        f"--model {alias} is ambiguous here. This session exports "
        f"{var}={remapped}, so {alias!r} resolves to that vendor's model id, "
        "while the worker's endpoint and credential are resolved separately "
        'and would not match it. The spawn would report "live" and then fail '
        "on its first turn. No worker launched.\n"
        "Name the route instead, so endpoint, auth, and model are chosen as "
        "one unit:\n"
        f"  --account <id> --model {alias}      # Anthropic's {alias} "
        "(ids from `fno config accounts list`)\n"
        f"  -P <vendor> --model {remapped}   # stay on the routed vendor "
        "(vendors from `fno route ls`)\n"
        f"Or unset {var} for this command."
    )


def check_spawn_tier_remap(
    provider: Optional[str],
    model: Optional[str],
    *,
    role: Optional[str] = None,
    route_env: Optional[Mapping[str, str]] = None,
    account_env: Optional[Mapping[str, str]] = None,
    env: Optional[Mapping[str, str]] = None,
    settings: "Optional[SettingsModel]" = None,
) -> None:
    """Raise :class:`TierRemapConflict` when a claude spawn would launch with a
    tier alias the ambient environment redefines and nothing composed a
    complete route. No-op otherwise.

    Called from the in-process spawn APIs as well as the CLI seam: the CLI is
    one of several reachable paths, so a guard only there is decorative.

    A resolved ``route_env`` or ``account_env`` composes endpoint, auth, and
    model as one unit and is exempt. ``role`` is NOT exempt on mention alone:
    :func:`resolve_route` is deliberately fail-SAFE (a protected role, a
    disabled block, an unconfigured provider, or a missing key all return
    ``None`` and leave the ambient environment untouched), so ``--role
    implement --model opus`` under a foreign remap recreates the exact failure
    this guards. Exempt a role only once it produced a real route.
    """
    if (provider or "claude") != "claude":
        return
    if route_env or account_env:
        return
    # Cheap env-only check first; the role resolution below reads config.
    found = tier_remap_conflict(model, env)
    if found is None:
        return
    if role and resolve_route(role, settings=settings, env=env):
        return
    raise TierRemapConflict(remap_conflict_message(*found))


# CLAUDE_CODE_SUBPROCESS_ENV_SCRUB. Set by an operator (or a hardened shell) to
# strip inherited ANTHROPIC_* env from a subprocess. Read at BOTH the CLI spawn
# seam and the in-process spawn APIs so every reachable path sees the warning.
ENV_SCRUB_VAR = "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB"


def env_scrub_truthy(env: Optional[Mapping[str, str]] = None) -> bool:
    """Whether CLAUDE_CODE_SUBPROCESS_ENV_SCRUB is set to a truthy value.

    Claude Code honors ``=0`` as the opt-out, so empty / ``0`` / ``false`` and
    the common off-words are falsy."""
    if env is None:
        env = os.environ
    return (env.get(ENV_SCRUB_VAR) or "").strip().lower() not in (
        "",
        "0",
        "false",
        "no",
        "off",
    )


def env_scrub_warning(
    provider: Optional[str],
    *,
    permission_pinned: bool,
    env: Optional[Mapping[str, str]] = None,
) -> Optional[str]:
    """The one stderr warning when a claude spawn pins a permission mode under
    CLAUDE_CODE_SUBPROCESS_ENV_SCRUB, else None.

    The var half-scrubs a vendor env (drops ANTHROPIC_AUTH_TOKEN, leaves
    ANTHROPIC_BASE_URL and the tier models) and silently forces an explicit
    permission mode to default, so an unattended worker stalls on approvals no
    one is present to give. This names both consequences and points at ``=0``.

    A warning, never a refusal: the var is the operator's security posture and a
    legitimate hardened setup must still spawn. Narrow, since the downgrade only
    contradicts a stated intent: a non-claude provider and a spawn naming no
    permission mode are left alone.

    Takes the RESOLVED provider so the in-process spawn APIs (which know the
    harness they were handed) judge the spawn on what it actually is; the CLI
    seam resolves it via its argv adapter, since a bare spawn defaults to the
    invoking harness rather than claude.
    """
    if (provider or "").strip().lower() != "claude":
        return None
    if not env_scrub_truthy(env):
        return None
    if not permission_pinned:
        return None
    return (
        f"fno agents spawn: {ENV_SCRUB_VAR} is set; it strips "
        "ANTHROPIC_AUTH_TOKEN but leaves ANTHROPIC_BASE_URL and the tier model "
        "vars (a half-composition), and silently forces --permission-mode to "
        "default over the one pinned here, so this worker may stall on "
        f"approvals. Set {ENV_SCRUB_VAR}=0 to keep the pinned mode."
    )


def emit_env_scrub_warning(
    provider: Optional[str],
    *,
    permission_pinned: bool,
    env: Optional[Mapping[str, str]] = None,
) -> None:
    """Surface the env-scrub warning from an in-process spawn path.

    Pairs with :func:`check_spawn_tier_remap` at the same call sites so the
    reachability is identical: the invariant that lives at the CLI seam also
    holds for in-process callers that bypass argument parsing. A warning, never
    a refusal."""
    msg = env_scrub_warning(provider, permission_pinned=permission_pinned, env=env)
    if msg is not None:
        import sys

        print(msg, file=sys.stderr)


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


def _resolve_key(provider: Mapping[str, Optional[str]], env: Mapping[str, str]) -> Optional[str]:
    """Resolve a provider's API key. Precedence: process env (named by
    ``api_key_env``) wins over the optional ``api_key_file``. Never raises."""
    key_name = provider.get("api_key_env") or ""
    if key_name:
        from_env = env.get(key_name)
        if from_env:
            return from_env
    key_file = provider.get("api_key_file")
    if key_file and key_name:
        return read_var_from_env_file(key_file, key_name)
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
        if read_var_from_env_file(str(key_file), key_name):
            return str(key_file), checked
    return None, checked


def resolve_route(
    role: Optional[str],
    *,
    settings: "Optional[SettingsModel]" = None,
    env: Optional[Mapping[str, str]] = None,
    notice: Optional[Callable[[str], object]] = None,
    business_lookup: Optional[BusinessRoleLookup] = None,
) -> Optional[dict[str, str]]:
    """Resolve per-spawn env overrides for ``role``.

    Returns a dict of env keys to merge into the worker's spawn env, or ``None``
    meaning "use the primary Anthropic model, change nothing".

    ``None`` is returned for: no role, a production/unrouted role, a disabled
    block, an unconfigured / non-Anthropic provider, or a missing key
    (fail-safe). The legacy path never raises. An opted-in business lookup
    raises a typed error when an existing business role is blocked or lacks a
    complete composable routing projection.

    Args:
        role: the spawn's role; case/space-insensitive.
        settings: a SettingsModel; loaded from config when None.
        env: environment mapping for key lookup; ``os.environ`` when None.
        notice: optional one-line-notice sink for fail-safe fallbacks.
    """
    requested_role = (role or "").strip()
    name = _normalize(requested_role)
    if not name:
        return None

    business_role = _resolve_business_role(requested_role, business_lookup)

    # A protected role never routes, even if config or a business manifest
    # asks it to. Discovery still runs first so a malformed manifest cannot
    # masquerade as ordinary protected-role fallback.
    if name in PROTECTED_ROLES:
        return None

    if env is None:
        import os

        env = os.environ

    block = _routing_block(settings)
    if not getattr(block, "enabled", True):
        return None

    target = _business_role_target(name, block, business_role)
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
            f"model-routing: provider {pname!r} for {ctx} is not configured; {fallback}",
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
            f"model-routing: no API key for provider {pname!r} ({ctx}); {fallback_key}",
        )
        return None

    route = {"ANTHROPIC_BASE_URL": base_url, "ANTHROPIC_AUTH_TOKEN": key}
    for k in MODEL_ENV_KEYS:
        route[k] = model
    # Item 1: route the background (haiku) tier to the provider's cheaper
    # haiku_model (zai -> glm-4.5-air). Still the SAME secondary provider
    # (base_url + token), so the whole worker stays off Anthropic; only the
    # background model is cheaper. A provider with no haiku_model keeps the role
    # model on the haiku tier (no regression, never an empty/invalid id).
    haiku_model = provider.get("haiku_model")
    if haiku_model:
        route["ANTHROPIC_DEFAULT_HAIKU_MODEL"] = haiku_model
    # Item 2: a [1m]-routed worker gets an auto-compact backstop. The [1m]
    # variant selects the 1M context; this threshold (capped at the model
    # window, and precedence over /autocompact/--autocompact/setting) is the
    # backstop above the king nudge. Injected before extra_env so an explicit
    # extra_env value still wins.
    if model.endswith("[1m]"):
        route["CLAUDE_CODE_AUTO_COMPACT_WINDOW"] = ONE_M_AUTO_COMPACT_WINDOW
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
    return _route_for_target(pname, model.strip(), block, env, notice, ctx=f"peer {pname!r}")


def materialize_route_settings(route_env: Mapping[str, str]) -> str:
    """Write ``route_env`` as a claude ``--settings`` JSON and return its path.

    A ``claude --bg`` session's serving process is forked by the claude daemon
    with the DAEMON's env, so per-spawn ``ANTHROPIC_*`` route env is dropped
    before the first model request and the session wedges on the primary model
    (x-6de8). A settings file is read by the session process itself, so it
    survives that fork where an env overlay cannot. Content-addressed and
    ``0600`` (it carries ``ANTHROPIC_AUTH_TOKEN``): identical routes reuse one
    file rather than accumulating per spawn.

    Every ``SCRUB_AUTH_VARS`` entry is written as an empty string FIRST, with
    the route on top. Without that floor a routed worker keeps whatever auth the
    invoking shell exported - and the env scrub that would have dropped it is
    itself discarded at the daemon fork - so a session pointed at another
    endpoint could still authenticate with an inherited ``ANTHROPIC_API_KEY``.
    A composed ``--route`` + ``--account`` spawn writes ONLY this file (the
    route wins the settings file by design), which is where that gap lived.
    """
    import hashlib
    import json
    import os

    from fno import paths
    from fno.agents.account_env import SCRUB_AUTH_VARS

    env: dict[str, str] = {var: "" for var in SCRUB_AUTH_VARS}
    env.update(route_env)
    payload = json.dumps({"env": env}, sort_keys=True)
    digest = hashlib.sha256(payload.encode("utf-8")).hexdigest()[:16]
    # Route through fno.paths (config-driven ~/.fno) rather than a bare
    # Path.home() -- the check-no-hardcoded-paths gate forbids the literal, and
    # the settings file should follow a config state_dir override like every
    # other sidecar.
    base = paths.state_dir() / "route-settings"
    base.mkdir(parents=True, exist_ok=True)
    path = base / f"{digest}.json"
    if not path.exists():
        # Publish atomically: two agents on the same route resolve to this same
        # content-addressed path, and a bare O_CREAT makes the file visible before
        # its write finishes -- a racing spawn could hand claude an empty/partial
        # settings file. Write a pid-unique temp then os.replace (atomic on the
        # same fs); content-addressing means racing writers write identical bytes,
        # so a last-writer-wins rename is still correct.
        tmp = base / f".{digest}.{os.getpid()}.tmp"
        fd = os.open(str(tmp), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        try:
            os.write(fd, payload.encode("utf-8"))
        finally:
            os.close(fd)
        os.replace(str(tmp), str(path))
    return str(path)


def materialize_account_scrub_settings(account_env: Mapping[str, str]) -> str:
    """Write an ``--account`` spawn's auth/model scrub as a claude ``--settings``
    JSON and return its path.

    The env scrub layered in ``bg_create``/``headless_create`` is dropped at the
    ``claude --bg`` daemon fork: the daemon forks the session with its OWN env,
    not the invoker's per-spawn overlay, so an account worker born under another
    vendor's exported ``ANTHROPIC_*`` inherits those vars and resolves the vendor
    model. A settings file is read by the forked session itself, so expressing
    the scrub here survives that fork where an env overlay cannot.

    Every ``SCRUB_AUTH_VARS`` entry is written as an empty string: claude reads
    an empty settings env value as UNSET (measured 2026-07-27), which is what
    drops the inherited vendor endpoint and model tiers. The account's FULL
    resolved overlay is then re-applied (not just the scrub vars), so an api_key
    account survives the fork with every env its record pins, including extra
    entries a scrub-var allowlist would drop (custom headers, proxy vars, ...).
    CLAUDE_CONFIG_DIR is the one exclusion: it selects which config the worker
    reads, so it cannot live in a file read FROM that config, and it rides the
    spawn env (which selects the per-account daemon).

    Content-addressed and 0600 via the shared writer, so identical account
    overlays reuse one file rather than accumulating per spawn.
    """
    from fno.agents.account_env import SCRUB_AUTH_VARS

    scrub = {var: "" for var in SCRUB_AUTH_VARS}
    overlay = {k: v for k, v in account_env.items() if k != "CLAUDE_CONFIG_DIR"}
    return materialize_route_settings({**scrub, **overlay})


class RouteRestoreError(RuntimeError):
    """A recorded route could not be read back, so a relaunch must refuse."""


def route_settings_path_for(
    route_env: Optional[Mapping[str, str]],
    account_env: Optional[Mapping[str, str]] = None,
) -> Optional[str]:
    """The ``--settings`` path a spawn carrying this overlay launches with.

    ONE place expressing the route-wins-the-settings-file precedence (x-5ed4),
    so the spawn seam and the registry row it stamps can never disagree about
    which file a worker was launched with. Both writers are content-addressed,
    so re-asking for an already-materialized overlay returns the same path
    rather than writing a second file - which is what lets a caller that only
    holds the resolved env recover the path without threading it through.
    """
    if route_env:
        return materialize_route_settings(route_env)
    if account_env:
        return materialize_account_scrub_settings(account_env)
    return None


def read_route_settings(path: str) -> dict[str, str]:
    """Read a recorded route-settings file back into the route it expressed.

    Returns the route's OWN keys only. The stored file is the auth-scrub floor
    (every ``SCRUB_AUTH_VARS`` entry as an empty string) with the route written
    on top, and an empty value means "unset" only to claude reading a settings
    FILE. Handing the floor back to a spawn would put it in the child's process
    environment instead, where the original launch had those vars simply absent
    - a revived worker carrying ``ANTHROPIC_API_KEY=""`` and every unset tier
    var is not running the env it ran under. The floor is re-applied by
    :func:`materialize_route_settings` when the relaunch writes its own settings
    file, so dropping it here loses nothing and keeps the replay faithful.

    Raises :class:`RouteRestoreError` when the file is gone, unreadable, or
    malformed. Every caller is a relaunch, and a relaunch that cannot restore
    its route must refuse rather than fall back: an unrouted worker WORKS, so
    it bills the default Anthropic account and reports nothing - the one
    failure shape an operator discovers from an invoice.
    """
    import json
    from pathlib import Path

    try:
        payload = json.loads(Path(path).read_text(encoding="utf-8"))
        env = payload["env"]
    except OSError as exc:
        raise RouteRestoreError(f"route settings file {path} is unreadable: {exc}") from exc
    except (ValueError, KeyError, TypeError) as exc:
        raise RouteRestoreError(f"route settings file {path} is malformed: {exc}") from exc
    if not isinstance(env, dict):
        raise RouteRestoreError(f"route settings file {path} has no env mapping")
    route = {str(k): str(v) for k, v in env.items() if str(v) != ""}
    if not route:
        # The row claims this worker was routed, and the file is readable but
        # carries only the scrub floor. Returning {} would read as "never routed"
        # to the caller and relaunch on the default account without a word - the
        # same silent fallback as a missing file, so it takes the same refusal.
        raise RouteRestoreError(f"route settings file {path} records no route")
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
    business_lookup: Optional[BusinessRoleLookup] = None,
) -> Optional[CodexRoute]:
    """Resolve codex-lane routing for ``role`` against an OpenAI-protocol
    provider, or ``None`` (use codex's default config, change nothing).

    Mirrors :func:`resolve_route`'s fail-safe contract: a protected role, a
    disabled block, an unrouted role, an unconfigured provider, a NON-openai
    provider (that belongs to the claude lane), a missing base_url/key, or a
    value that can't be safely embedded in TOML all return ``None`` (with a
    one-line notice where a misconfiguration is worth surfacing). The legacy
    path never raises; an opted-in business lookup fails closed with the same
    typed errors as the Claude lane.
    """
    requested_role = (role or "").strip()
    name = _normalize(requested_role)
    if not name:
        return None

    business_role = _resolve_business_role(requested_role, business_lookup)
    if name in PROTECTED_ROLES:
        return None

    if env is None:
        import os

        env = os.environ

    block = _routing_block(settings)
    if not getattr(block, "enabled", True):
        return None

    target = _business_role_target(name, block, business_role)
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
    eff: dict[str, str] = {r: f"zai,{DEFAULT_SECONDARY_MODEL}" for r in DEFAULT_ROUTED_ROLES}
    for k, v in (getattr(block, "roles", None) or {}).items():
        eff[str(k).strip().lower()] = str(v)
    return eff


def _role_target(role: str, block: "ModelRoutingBlock") -> Optional[tuple[str, str]]:
    """Resolve (provider, model) for a routed ``role``, or None if not routed."""
    raw = _effective_roles(block).get(role)
    if not raw:
        return None
    return _parse_target(str(raw))


def _resolve_business_role(
    role: str,
    business_lookup: Optional[BusinessRoleLookup],
) -> "Optional[ResolvedRole | ManifestRoutingResolution]":
    """Validate the production business-role tri-state before routing policy."""
    if business_lookup is None:
        business_lookup = _default_business_lookup

    from fno.roles import (
        ManifestRoutingResolution,
        ResolvedRole,
        RoleResolutionBlocked,
        RoleResolutionReason,
    )

    result = business_lookup(role)
    if isinstance(result, RoleResolutionBlocked):
        if result.reason is RoleResolutionReason.NOT_FOUND:
            return None
        raise BusinessRoleResolutionBlockedError(result)
    if not isinstance(result, (ResolvedRole, ManifestRoutingResolution)):
        raise BusinessRoleRoutingProjectionError(
            f"business lookup for role {role!r} returned no typed resolution; no worker launched"
        )
    return result


def _default_business_lookup(
    role: str,
) -> "ManifestRoutingResolution | RoleResolutionBlocked":
    """Discover conventional manifests only when a role root actually exists."""
    from pathlib import Path
    import stat

    from fno.company.contracts import RoleRef
    from fno.roles import (
        RoleResolutionBlocked,
        RoleResolutionReason,
        default_role_root,
        discover_role_definitions,
        resolve_manifest_routing,
    )

    configured_root = os.environ.get("FNO_ROLES_ROOT")
    root = Path(configured_root).expanduser() if configured_root else default_role_root()
    try:
        root_status = root.stat()
    except FileNotFoundError:
        if configured_root:
            return RoleResolutionBlocked(
                role=RoleRef(id=role, function_id="unavailable"),
                reason=RoleResolutionReason.INVALID_MANIFEST,
                source_id=str(root),
                reference=role,
                detail="configured role root does not exist",
            )
        return RoleResolutionBlocked(
            role=RoleRef(id=role, function_id="unavailable"),
            reason=RoleResolutionReason.NOT_FOUND,
            reference=role,
        )
    except OSError as exc:
        return RoleResolutionBlocked(
            role=RoleRef(id=role, function_id="unavailable"),
            reason=RoleResolutionReason.INVALID_MANIFEST,
            source_id=str(root),
            reference=role,
            detail=f"{type(exc).__name__}: {exc}",
        )
    if not stat.S_ISDIR(root_status.st_mode):
        return RoleResolutionBlocked(
            role=RoleRef(id=role, function_id="unavailable"),
            reason=RoleResolutionReason.INVALID_MANIFEST,
            source_id=str(root),
            reference=role,
            detail="role root exists but is not a directory",
        )
    return resolve_manifest_routing(role, discover_role_definitions(root=root))


def _business_role_target(
    role: str,
    block: "ModelRoutingBlock",
    business_role: "Optional[ResolvedRole | ManifestRoutingResolution]",
) -> Optional[tuple[str, str]]:
    """Project only provider/model material into the legacy routing target."""
    legacy_target = _role_target(role, block)
    if business_role is None:
        return legacy_target

    projection = business_role.routing_projection
    if projection is None:
        return None
    provider = projection.provider if projection is not None else None
    model = projection.model if projection is not None else None
    if legacy_target is not None:
        provider = provider or legacy_target[0]
        model = model or legacy_target[1]

    missing = [
        field for field, value in (("provider", provider), ("model", model)) if value is None
    ]
    if missing:
        fields = " and ".join(missing)
        raise BusinessRoleRoutingProjectionError(
            f"resolved business role {role!r} has an incomplete routing projection: "
            f"missing {fields}; no configured legacy target can complete it; "
            "no worker launched"
        )
    assert provider is not None and model is not None
    return provider, model


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


def _resolve_provider(pname: str, block: "ModelRoutingBlock") -> Optional[dict[str, Optional[str]]]:
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
    canonical 5-field shape: ``role | provider_model | protocol | key | assigned_by``
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
    config_roles = {str(k).strip().lower() for k in (getattr(block, "roles", None) or {})}

    names: list[str] = []
    for r in (
        *DEFAULT_ROUTED_ROLES,
        *KNOWN_LANE_ROLES,
        *sorted(config_roles),
        *sorted(PROTECTED_ROLES),
    ):
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
                    "provider_model": "never routed (hard guard)",
                    "protocol": "-",
                    "key": "-",
                    "assigned_by": "protected (hard guard)",
                }
            )
            continue
        source = (
            "config"
            if role in config_roles
            else ("built-in" if role in DEFAULT_ROUTED_ROLES else "known lane")
        )
        raw = eff.get(role)
        if not raw:
            rows.append(
                {
                    "role": role,
                    "provider_model": "unconfigured",
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
                    "provider_model": f"{raw} (malformed)",
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
                    "provider_model": f"{pname}/{model}",
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
                "provider_model": f"{pname}/{model}",
                "protocol": protocol,
                "key": key_status,
                "assigned_by": _provenance(role, source),
            }
        )
    return rows
