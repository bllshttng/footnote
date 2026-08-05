"""Combo-aware dispatch target resolution (CG8, Plan B).

The single home for ``DispatchTarget`` and ``resolve_dispatch_target()``,
relocated out of the deleted ``sigma_dispatch`` module: that module existed to
emit ``subagent_spawn``/``subagent_complete`` events for a sigma-dispatch
verifier that no path ever reached, so it was removed for cause. This resolver
is the live half - it decides which provider/combo an agent dispatches on - and
it has real callers in the agent routing surfaces, so it stays.

Precedence (highest priority first):
  1. Per-agent pin: config.agents.<name>.provider - single provider.
  2. TARGET_COMBO env var: rotate via dispatch_with_combo(combo_name, fn).
  3. settings active_combo: config.providers.active_combo.
  4. settings active provider: config.providers.active.

Per-agent pin wins over combo when both are set for the same agent; combos
compose with per-agent routing as additional fallback, not replacement.
"""
from __future__ import annotations

import dataclasses
import logging
import os
from pathlib import Path


@dataclasses.dataclass(frozen=True)
class DispatchTarget:
    """What the orchestrator should do for one subagent dispatch.

    Exactly one of ``provider_id`` or ``combo_name`` is set. ``source``
    names which precedence rule fired ('per_agent_pin', 'env_combo',
    'settings_combo', 'active_provider') for forensics + tests.
    """

    provider_id: str | None = None
    combo_name: str | None = None
    source: str = ""


def resolve_dispatch_target(
    agent_name: str,
    *,
    repo_root: Path | None = None,
    env: dict[str, str] | None = None,
) -> DispatchTarget:
    """Resolve the dispatch target for ``agent_name`` per CG8 precedence.

    Combo lookups use ``load_combos``; an unknown combo (TARGET_COMBO points
    to a deleted combo, or settings.yaml's active_combo names a missing
    combo) is logged at WARNING and falls through to the next rule rather
    than raising - this matches AC8.3's "fall through to active provider"
    contract.

    Note on the precedence chain (PR #230 review H2): the design doc lists
    five tiers (per-agent pin > skill modifier > env > settings active_combo
    > active provider), but skill modifier and env collapse to a single
    ``TARGET_COMBO`` slot here by design. Both surfaces (skill ``combo
    <name>`` modifier and ``--combo`` CLI flag) write to the same env var
    with last-writer-wins semantics; the conceptual five-tier chain is
    preserved by the SUM of write paths even though the read side sees
    one env tier. ``DispatchTarget.source = "env_combo"`` covers both.
    If a future spec needs the skill modifier to override an inherited
    env, introduce a distinct ``TARGET_COMBO_SKILL`` env var checked
    before ``TARGET_COMBO``.
    """
    from fno.adapters.providers.loader import (
        load_combos,
        load_providers,
    )

    log = logging.getLogger(__name__)
    env_view = env if env is not None else os.environ
    root = repo_root or Path.cwd()

    # 1. Per-agent pin (Spec 3) wins over everything.
    try:
        config = load_providers(repo_root=root)
    except Exception as exc:  # ProviderConfigError or similar
        log.warning("resolve_dispatch_target: load_providers failed: %s", exc)
        config = None

    if config is not None:
        binding = config.agents.get(agent_name)
        if binding is not None and binding.provider in config.by_id:
            return DispatchTarget(
                provider_id=binding.provider, source="per_agent_pin"
            )

    # 2. TARGET_COMBO env var (set by --combo flag, skill modifier, or manifest).
    env_combo = env_view.get("TARGET_COMBO")
    try:
        combos = load_combos(repo_root=root)
    except Exception as exc:
        log.warning("resolve_dispatch_target: load_combos failed: %s", exc)
        combos = {}

    if env_combo:
        if env_combo in combos:
            return DispatchTarget(combo_name=env_combo, source="env_combo")
        log.warning(
            "resolve_dispatch_target: TARGET_COMBO=%r not found in combos %s; "
            "falling through to next precedence rule.",
            env_combo,
            sorted(combos),
        )

    # 3. settings active_combo - project-local-over-global, mirroring
    #    load_providers/load_combos. Walk both roots (config.toml, else legacy
    #    settings.yaml) so a global combo is reachable when no project-local
    #    override exists. (PR #230 Gemini MEDIUM #2: the previous
    #    implementation only inspected project-local.)
    from fno.config import config_read_candidates, read_config_flat

    active_combo: str | None = None
    for candidate in config_read_candidates([
        root / ".fno" / "settings.yaml",
        # Bootstrap: cannot use paths.config_file() here (settings loader self-reference)
        Path.home() / ".fno" / "settings.yaml",
    ]):
        if not candidate.is_file():
            continue
        flat = read_config_flat(candidate)
        # Canonical `accounts`, else the pre-rename `providers`. Bootstrap read,
        # so it never reaches the providers loader's choke point; a miss yields
        # active_combo=None, which silently degrades routing rather than erroring.
        block = flat.get("accounts") or flat.get("providers")
        ac = block.get("active_combo") if isinstance(block, dict) else None
        if ac:
            active_combo = ac
            break  # project-local wins; do not consult global

    if active_combo and active_combo in combos:
        return DispatchTarget(combo_name=active_combo, source="settings_combo")
    if active_combo and active_combo not in combos:
        # Configured but unknown - log distinctly so operators can tell this
        # apart from "no active_combo configured at all". Falls through to
        # the active-provider branch (matches the env_combo path's
        # log-and-fall-through behavior).
        log.warning(
            "resolve_dispatch_target: settings active_combo=%r not in "
            "loaded combos %s; falling through to active provider.",
            active_combo, sorted(combos),
        )

    # 4. Fall through to active provider.
    if config is not None and config.active and config.active in config.by_id:
        return DispatchTarget(provider_id=config.active, source="active_provider")

    return DispatchTarget(source="unresolved")
