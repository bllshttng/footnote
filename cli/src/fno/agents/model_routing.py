"""Role-based per-spawn model routing for fno agents (x-d2fe).

Cheap coordination work (backlog tidying, node orientation, memory
consolidation) is routed to z.ai's GLM via the worker's *environment* at
spawn time; production work (writing the diff, the correctness verdict)
stays on the default Anthropic model, byte-for-byte as today.

Mechanism (Locked Decision 2): a spawn stamps ``ANTHROPIC_BASE_URL`` +
``ANTHROPIC_AUTH_TOKEN`` + ``ANTHROPIC_MODEL`` into the worker env. No proxy
in the critical path; each worker is a fresh process, so switching base_url
per spawn is safe (Failure Modes: never switch base_url mid-session).

Two non-negotiable invariants:

- **Fail safe, not fail closed** (US4): if no z.ai key is configured, a cheap
  role falls back to the default Anthropic model with a one-line notice and
  the spawn still succeeds. :func:`resolve_route` never raises.
- **Hard quality guard** (Failure Modes): ``implement`` / ``review-verdict``
  never resolve to a cheap provider, even via a config override. The guard
  short-circuits before any override is read.

Only the z.ai (``zai``) lane is wired in v1. An override naming another cheap
provider degrades to the default model rather than erroring, keeping the
multi-provider story (US6) forward-compatible without a CCR dependency.
"""
from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING, Callable, Mapping, Optional

if TYPE_CHECKING:
    from fno.config import ModelRoutingBlock, SettingsModel

# z.ai's Anthropic-compatible endpoint (verified live on claude 2.1.170).
ZAI_BASE_URL = "https://api.z.ai/api/anthropic"

# Default cheap model. glm-4.5-air is too weak for reasoning-bearing work
# (cosmetic noise); glm-5.1 does real work, so it is the default for every
# cheap role. Trivial classification can be pinned to glm-4.5-air via override.
DEFAULT_CHEAP_MODEL = "glm-5.1"

# Roles that go cheap (z.ai GLM) by default. The money model never touches
# these; they shuffle the backlog and consolidate memory.
CHEAP_ROLES = frozenset({"coordinate", "tidy", "orient", "consolidate"})

# Roles the cheap provider must NEVER touch (writes a diff / renders a
# correctness verdict). Hard guard, enforced before any config override.
PROTECTED_ROLES = frozenset({"implement", "review-verdict"})


def _emit(notice: Optional[Callable[[str], object]], message: str) -> None:
    """Surface a one-line fail-safe notice; quiet when no sink is supplied."""
    if notice is not None:
        notice(message)


def _normalize(role: Optional[str]) -> str:
    return (role or "").strip().lower()


def _parse_override(raw: str) -> Optional[tuple[str, str]]:
    """Parse a ``"provider,model"`` override value into a (provider, model)
    pair. Returns None for a malformed value (fail-safe; caller degrades)."""
    parts = [p.strip() for p in raw.split(",")]
    if len(parts) != 2 or not parts[0] or not parts[1]:
        return None
    return parts[0].lower(), parts[1]


def _key_from_env_file(path_str: str, key_name: str) -> Optional[str]:
    """Read ``key_name`` from a ``.env``-style file. Missing file / key is not
    fatal: returns None so the caller falls back to the default model."""
    try:
        text = Path(path_str).expanduser().read_text(encoding="utf-8")
    except (OSError, ValueError):
        return None
    prefix = f"{key_name}="
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("#") or "=" not in line:
            continue
        if line.startswith(prefix):
            return line[len(prefix):].strip().strip('"').strip("'") or None
    return None


def _resolve_zai_key(
    block: "ModelRoutingBlock", env: Mapping[str, str]
) -> Optional[str]:
    """Resolve the z.ai key. Precedence mirrors modelkit: process env wins
    over the optional ``.env`` file. Never hardcoded; never raises."""
    key_name = getattr(block, "zai_key_env", "ZAI_API_KEY") or "ZAI_API_KEY"
    from_env = env.get(key_name)
    if from_env:
        return from_env
    env_file = getattr(block, "zai_env_file", None)
    if env_file:
        return _key_from_env_file(env_file, key_name)
    return None


def resolve_route(
    role: Optional[str],
    *,
    settings: "Optional[SettingsModel]" = None,
    env: Optional[Mapping[str, str]] = None,
    notice: Optional[Callable[[str], object]] = None,
) -> Optional[dict[str, str]]:
    """Resolve per-spawn env overrides for ``role``.

    Returns a dict of ``ANTHROPIC_*`` env keys to merge into the worker's
    spawn env, or ``None`` meaning "use the default model, change nothing".

    ``None`` is returned (and the spawn stays on the default Anthropic model)
    for: no role, a production/unknown role, a disabled routing block, a
    missing key (fail-safe), or an unwired cheap provider. Only a cheap role
    with a configured key returns a route. This function never raises.

    Args:
        role: the spawn's role; case/space-insensitive.
        settings: a SettingsModel; loaded from config when None.
        env: environment mapping for key lookup; ``os.environ`` when None.
        notice: optional one-line-notice sink for fail-safe fallbacks.
    """
    name = _normalize(role)
    if not name:
        return None

    # Hard quality guard FIRST: a protected role is never cheap, even if a
    # config override tries to route it. Short-circuits before reading config.
    if name in PROTECTED_ROLES:
        return None

    if env is None:
        import os

        env = os.environ

    block = _routing_block(settings)
    if not getattr(block, "enabled", True):
        return None

    # Provider + model: a config override wins, else the default cheap policy.
    provider, model = _provider_and_model(name, block)
    if provider is None or model is None:
        # Not a cheap role (and not overridden into one) -> default model.
        return None

    if provider != "zai":
        # Only the z.ai lane is wired in v1; degrade rather than error so the
        # multi-provider story stays forward-compatible (US6 / no CCR dep).
        _emit(
            notice,
            f"model-routing: provider {provider!r} not wired for role "
            f"{name!r}; using default model",
        )
        return None

    key = _resolve_zai_key(block, env)
    if not key:
        _emit(
            notice,
            f"model-routing: no z.ai key for role {name!r}; "
            f"falling back to the default Anthropic model",
        )
        return None

    return {
        "ANTHROPIC_BASE_URL": ZAI_BASE_URL,
        "ANTHROPIC_AUTH_TOKEN": key,
        "ANTHROPIC_MODEL": model,
    }


def _routing_block(settings: "Optional[SettingsModel]") -> "ModelRoutingBlock":
    """Return the model_routing config block, loading settings if needed."""
    if settings is None:
        from fno.config import load_settings

        settings = load_settings()
    return settings.config.model_routing


def _provider_and_model(
    role: str, block: "ModelRoutingBlock"
) -> tuple[Optional[str], Optional[str]]:
    """Resolve (provider, model) for a cheap ``role``.

    An override (``role -> "provider,model"`` in config) wins over the default
    cheap policy. A non-cheap, non-overridden role returns (None, None) so the
    caller keeps the default model.
    """
    overrides = getattr(block, "overrides", None) or {}
    raw = overrides.get(role)
    if raw:
        parsed = _parse_override(str(raw))
        if parsed is not None:
            return parsed
        # Malformed override: fall through to the default policy below.
    if role in CHEAP_ROLES:
        return "zai", DEFAULT_CHEAP_MODEL
    return None, None
