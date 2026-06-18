"""Per-agent provider resolution for the cross-model review panel (ab-6c8f4c61).

Resolves which provider kind (``claude`` | ``codex`` | ``gemini``) each review
agent runs on, given the operator's ``config.review.agent_providers`` map and
the implementer's own provider (read from the ledger). The three correctness
agents cross-model to a provider that *differs from the implementer's* by
default; the operator can pin any agent to any provider.

Two layers:

* ``resolve_agent_provider`` - a PURE function over explicit inputs (the map,
  the implementer's kind, the ordered list of available kinds). This is what
  the AC suite drives with fakes; it never does I/O and never raises.
* ``load_implementer_provider`` / ``available_provider_kinds`` - thin I/O
  wrappers that feed the pure function from the real ledger + provider
  substrate. They REUSE ``adapters.providers`` (loader + runtime_state); they
  never build a parallel provider list (Domain Pitfall).

Graceful degradation is the rule: when no differing/available provider exists,
the agent runs on claude and the result is flagged ``degraded`` so the report
can say "cross-model unavailable" rather than silently appearing cross-modeled.
Cross-model is never a hard error (Locked Decision 4).
"""
from __future__ import annotations

import json
import logging
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Sequence

log = logging.getLogger(__name__)

CLAUDE = "claude"
ALTERNATE = "alternate"

# Provider kinds the panel can actually dispatch to: claude -> claude_runner,
# codex/gemini -> agents_spawn_runner. openclaw/hermes have no review runner so
# they are not dispatchable here even though the provider substrate knows them.
DISPATCHABLE_PROVIDERS: tuple[str, ...] = (CLAUDE, "codex", "gemini")

# Curated default applied when agent_providers is unset (an empty map): the
# three correctness-focused agents cross-model via `alternate`; every other
# agent stays on claude (Design "Per-agent provider map" + Locked Decision 2).
DEFAULT_ALTERNATE_AGENTS: frozenset[str] = frozenset(
    {"code_reviewer", "silent_failure_hunter", "type_design_analyzer"}
)


@dataclass(frozen=True)
class ResolvedProvider:
    """The provider one review agent resolves to, plus attribution context.

    ``provider`` is always one of :data:`DISPATCHABLE_PROVIDERS`. ``degraded``
    is True when the operator's intended cross-model run could not happen and
    the agent fell back to claude (single-provider env, all alternates locked
    out, or an unknown provider literal). ``reason`` carries a short note the
    report renders next to the agent.
    """

    provider: str
    degraded: bool = False
    reason: str | None = None


def _requested_for(
    agent: str, agent_providers: dict[str, str]
) -> str:
    """Pick the raw requested provider for ``agent`` before normalization.

    - agent named in the map        -> the mapped value (literal or `alternate`)
    - map empty (unset)             -> curated default (correctness -> alternate)
    - map set but agent not named   -> claude (unnamed agents stay on claude)
    """
    if agent in agent_providers:
        return str(agent_providers[agent]).strip().lower()
    if not agent_providers:
        return ALTERNATE if agent in DEFAULT_ALTERNATE_AGENTS else CLAUDE
    return CLAUDE


def resolve_agent_provider(
    agent: str,
    *,
    agent_providers: dict[str, str],
    implementer_provider: str,
    available_providers: Sequence[str],
    known_agents: Sequence[str] | None = None,
) -> ResolvedProvider:
    """Resolve the provider kind for a single review ``agent``. Never raises.

    Args:
        agent: agent name, e.g. ``"code_reviewer"``.
        agent_providers: operator map ``agent -> provider``. Empty map applies
            the curated correctness-subset default.
        implementer_provider: the provider kind that wrote the code (from the
            ledger; ``"claude"`` when unknown). Excluded when resolving
            ``alternate`` so cross-model genuinely means a different model.
        available_providers: ordered provider kinds available right now
            (rotation order, already lockout-filtered by the caller). Should
            always include ``claude`` (the local-runtime fallback).
        known_agents: the panel's agent names. A map key not in this set is an
            operator typo: warn + ignore (the agent resolves as if unnamed).

    Returns:
        :class:`ResolvedProvider`.
    """
    # An agent_providers key naming an unknown agent is a no-op typo: warn once
    # and let the *named* agent resolve normally. (We cannot drop the bad key
    # mid-resolution for THIS agent because we are resolving one agent at a
    # time; the warning is emitted by the selector when it iterates the map.
    # Here we only need to make sure an unknown `agent` argument still resolves
    # sanely - it falls through to the unnamed/default path.)
    if known_agents is not None and agent not in known_agents:
        log.warning(
            "cross-model: resolving unknown agent %r (not in the panel); "
            "treating as default-routed",
            agent,
        )

    requested = _requested_for(agent, agent_providers)

    # Claude pin (explicit or default for an unnamed/non-correctness agent):
    # no cross-model, no degradation.
    if requested == CLAUDE:
        return ResolvedProvider(provider=CLAUDE, degraded=False)

    # Literal codex/gemini pin: pins unconditionally (lockout is handled at
    # dispatch time by the selector's fall-through, not here).
    if requested in DISPATCHABLE_PROVIDERS:
        return ResolvedProvider(provider=requested, degraded=False)

    # `alternate`: pick the first available kind that differs from the
    # implementer and is dispatchable. Excluding the implementer is the
    # cross-model invariant.
    if requested == ALTERNATE:
        for candidate in available_providers:
            cand = str(candidate).strip().lower()
            if cand not in DISPATCHABLE_PROVIDERS:
                continue
            if cand == implementer_provider:
                continue
            return ResolvedProvider(provider=cand, degraded=False)
        # Nothing differs from the implementer -> degrade to claude.
        return ResolvedProvider(
            provider=CLAUDE,
            degraded=True,
            reason="cross-model unavailable: ran on claude",
        )

    # Unknown provider literal (e.g. "grok"): operator misconfig. Warn + run on
    # claude, flagged so the report shows the intended cross-model did not run.
    log.warning(
        "cross-model: agent %r mapped to unknown provider %r; running on "
        "claude (known: %s)",
        agent,
        requested,
        ", ".join(DISPATCHABLE_PROVIDERS),
    )
    return ResolvedProvider(
        provider=CLAUDE,
        degraded=True,
        reason=f"unknown provider {requested!r}: ran on claude",
    )


# ---------------------------------------------------------------------------
# I/O wrappers (production wiring) - thin, defensive, reuse the substrate.
# ---------------------------------------------------------------------------


def load_implementer_provider(
    session_id: str,
    *,
    ledger_path: Path | None = None,
) -> str:
    """Return the provider kind the implementer ran on for ``session_id``.

    Reads the ledger (``{"entries": [...]}``; each row carries ``session_id``
    and ``provider_id``, written by ``finalize``). The latest matching row
    wins. The ``provider_id`` is mapped to a dispatchable kind: a value that is
    already a kind (claude/codex/gemini) is used directly; otherwise it is
    looked up in ``config.providers.records`` by id and its ``cli`` is used.

    Absent ledger / no matching row / any error -> ``"claude"`` (OQ1: assume
    claude). Never raises.
    """
    try:
        if ledger_path is None:
            from fno import paths as _paths

            ledger_path = _paths.ledger_json()
        ledger_path = Path(ledger_path)
        if not ledger_path.is_file():
            return CLAUDE
        data = json.loads(ledger_path.read_text(encoding="utf-8"))
        entries = data.get("entries") if isinstance(data, dict) else None
        if not isinstance(entries, list):
            return CLAUDE
        provider_id: str | None = None
        for row in entries:
            if isinstance(row, dict) and row.get("session_id") == session_id:
                pid = row.get("provider_id")
                if isinstance(pid, str) and pid:
                    provider_id = pid  # latest match wins
        if not provider_id:
            return CLAUDE
        return _provider_id_to_kind(provider_id)
    except Exception as exc:  # noqa: BLE001 - never let a bad ledger break review
        log.warning("cross-model: implementer-provider read failed: %s", exc)
        return CLAUDE


def _provider_id_to_kind(provider_id: str) -> str:
    """Map a ledger provider_id to a dispatchable kind. Best-effort -> claude."""
    pid = provider_id.strip().lower()
    if pid in DISPATCHABLE_PROVIDERS:
        return pid
    try:
        from fno.adapters.providers.loader import load_providers

        record = load_providers().by_id.get(provider_id)
        if record is not None and record.cli in DISPATCHABLE_PROVIDERS:
            return record.cli
    except Exception as exc:  # noqa: BLE001
        log.warning("cross-model: provider_id->kind lookup failed: %s", exc)
    return CLAUDE


def available_provider_kinds(
    *,
    is_locked_out: Callable[[str], bool] | None = None,
    repo_root: Path | None = None,
) -> list[str]:
    """Return the ordered dispatchable provider kinds available right now.

    Derived from ``config.providers.records`` (declared order = rotation
    order): a kind is available when at least one record of that ``cli`` is not
    locked out. ``claude`` is always included as the local-runtime fallback
    (its availability is not gated on a provider record). When
    ``config.providers`` is empty/absent, returns ``["claude"]`` so
    ``alternate`` degrades cleanly (AC4-EDGE).

    REUSES ``loader.load_providers`` + ``runtime_state.is_in_cooldown`` - never
    a parallel provider list (Domain Pitfall). Never raises.
    """
    if is_locked_out is None:
        from fno.adapters.providers.runtime_state import is_in_cooldown

        is_locked_out = is_in_cooldown

    kinds: list[str] = [CLAUDE]
    try:
        from fno.adapters.providers.loader import load_providers

        records = load_providers(repo_root).records
    except Exception as exc:  # noqa: BLE001
        log.warning("cross-model: provider load failed; claude-only: %s", exc)
        return kinds

    for record in records:
        kind = record.cli
        if kind not in DISPATCHABLE_PROVIDERS or kind in kinds:
            continue
        try:
            locked = is_locked_out(record.id)
        except Exception:  # noqa: BLE001 - a bad cooldown read never hides a provider
            locked = False
        if not locked:
            kinds.append(kind)
    return kinds
