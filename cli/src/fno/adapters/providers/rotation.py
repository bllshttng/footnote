"""Provider combos: named ordered lists with rotation strategies.

Plan B of the 9router port (ab-0e5a921e). Composes on top of Plan A
(ab-6534a78a)'s ProviderHealth + cooldown substrate.

A Combo is a named ordered list of provider IDs with a strategy:

* ``fallback``: sequential try-next-on-error (the default; matches
  pre-combo single-provider semantics when the list has one entry).
* ``round_robin``: time-sliced cycle with a per-combo cursor that
  sticks for ``sticky_limit`` calls before advancing. Cursor lives in
  ``provider-runtime-state.json`` so parallel target spawns within a
  megawalk campaign share rotation state via fcntl locking.

Source port: ``~/code/tools/9router/open-sse/services/combo.js`` (MIT).
"""
from __future__ import annotations

import dataclasses
import hashlib
import logging
import time
from typing import Any, Callable, Literal, Union

from fno.adapters.providers.error_taxonomy import classify_error
from fno.adapters.providers.model import ProviderConfigError

logger = logging.getLogger(__name__)


_VALID_STRATEGIES = ("fallback", "round_robin")


class ComboNotFoundError(ProviderConfigError):
    """Raised when a referenced combo does not exist at dispatch time.

    Distinct from generic ProviderConfigError so callers (sigma_dispatch,
    CLI commands) can catch it and fall through to the no-combo path
    without swallowing other config-shape errors.
    """


@dataclasses.dataclass(frozen=True)
class Combo:
    """Named ordered provider list with rotation strategy.

    Loaded from ``config.providers.combos.<name>`` in settings.yaml.
    The ``providers`` tuple elements MUST be valid IDs from
    ``config.providers.records`` - cross-validated by ``load_combos``.
    """

    name: str
    strategy: Literal["fallback", "round_robin"] = "fallback"
    sticky_limit: int = 1
    providers: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        if not self.providers:
            raise ValueError(
                f"Combo {self.name!r} has empty providers list (combos must "
                "name at least one provider)"
            )
        if self.strategy not in _VALID_STRATEGIES:
            raise ValueError(
                f"Combo {self.name!r} has invalid strategy "
                f"{self.strategy!r}; valid: {_VALID_STRATEGIES}"
            )
        # Clamp sticky_limit to 1 minimum (matches 9router's
        # normalizeStickyLimit at combo.js:14-17).
        if self.sticky_limit < 1:
            object.__setattr__(self, "sticky_limit", 1)


def compute_providers_hash(providers: tuple[str, ...]) -> str:
    """Stable short hash of an ordered providers tuple.

    Used by the cursor system to detect mid-session combo edits: when
    the user adds/removes/reorders providers, the stored cursor's hash
    no longer matches and the cursor resets cleanly to index 0.
    Order-sensitive because rotation is meaningful only relative to a
    fixed order.
    """
    payload = "|".join(providers).encode("utf-8")
    return hashlib.sha256(payload).hexdigest()[:16]


# ---------------------------------------------------------------------------
# Dispatch result types + dispatch_with_combo (CG3).
#
# Caller-visible wire format. The ``fn`` callback returns CallOutcome; the
# loop interprets ``success``/``swap_trigger`` to decide whether to advance
# or surface. ``QueueExhausted`` is returned (not raised) so callers can
# pattern-match against the fallback path without exception-control-flow.
# ---------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class CallOutcome:
    """Result of one dispatch attempt against a single provider.

    ``payload`` is opaque - dispatch_with_combo passes it through unchanged
    when ``success`` is True. ``status``/``body`` carry context for error
    classification (matches Plan A's ``classify_error`` signature: HTTP
    status int and short body excerpt). ``swap_trigger`` is the caller's
    own classification of "this failure should rotate to the next provider"
    so callers that already know they hit a 5xx don't need this loop to
    re-classify - dispatch_with_combo trusts the caller's signal and
    additionally calls Plan A's ``classify_error`` to update health state.
    """

    success: bool
    payload: Any = None
    swap_trigger: bool = False
    status: int | None = None
    body: str | None = None


@dataclasses.dataclass(frozen=True)
class QueueExhausted:
    """Returned by dispatch_with_combo when no provider could serve the call.

    ``last_outcome`` is the last attempted provider's CallOutcome (None if
    every provider was skipped via cooldown). ``retry_after`` is the soonest
    epoch-seconds at which a cooldowned provider becomes eligible again.
    """

    last_outcome: CallOutcome | None = None
    retry_after: float | None = None


DispatchOutcome = Union[CallOutcome, QueueExhausted]


def _rotate_at(providers: tuple[str, ...], idx: int) -> list[str]:
    """Pure: return providers rotated so element at idx is at position 0."""
    return list(providers[idx:]) + list(providers[:idx])


def get_rotated_providers(combo: Combo) -> list[str]:
    """Return the provider list to try next, in order. ALSO advances cursor.

    Kept as a single-shot helper for callers that want one rotation +
    bump in one call (e.g., ad-hoc tools, the CLI list view's cursor
    preview). Production dispatch goes through ``dispatch_with_combo``
    which separates the read from the advance so cooldown-skips don't
    burn sticky slots that no provider actually served (PR #230 review
    H1).
    """
    from fno.adapters.providers.runtime_state import advance_cursor

    if combo.strategy != "round_robin" or len(combo.providers) <= 1:
        return list(combo.providers)
    providers_hash = compute_providers_hash(combo.providers)
    cursor = advance_cursor(
        combo.name,
        sticky_limit=combo.sticky_limit,
        providers_hash=providers_hash,
        providers_count=len(combo.providers),
    )
    return _rotate_at(combo.providers, cursor.cursor_index)


def dispatch_with_combo(
    combo_name: str,
    fn: Callable[[str], CallOutcome],
) -> DispatchOutcome:
    """Try providers from ``combo_name`` in rotation order until one succeeds.

    Iteration semantics (port of 9router's handleComboChat at combo.js:
    98-180):

    1. Resolve combo via ``load_combos()``; raise ``ComboNotFoundError`` if
       missing - callers may catch this and fall through to the no-combo
       single-provider path (sigma_dispatch does exactly this).
    2. Compute the rotated provider order via ``get_rotated_providers``.
    3. For each provider in order: re-check ``is_in_cooldown`` AT EACH STEP
       (no upfront snapshot - covers AC3.3 mid-iteration cooldown expiry).
       Skip on cooldown.
    4. Call ``fn(provider_id)``. On success: call ``record_success`` (Plan
       A's failover.record_success resets that provider's backoff state)
       and return the outcome. On swap-trigger failure: classify via
       ``classify_error`` + ``update_provider_health`` (Plan A's
       runtime_state writer), then continue to the next provider.
    5. On non-swap-trigger failure (caller's swap_trigger=False): return
       the outcome immediately so the caller can surface the error.
    6. If the loop exits with no success: return QueueExhausted with the
       last attempted outcome and the soonest cooldown-expiry hint.
    """
    # Local imports avoid an import cycle: rotation -> loader -> rotation
    # for the Combo type. loader.load_combos already does the same trick
    # (imports Combo from rotation inside the function body).
    from fno.adapters.providers.failover import record_success
    from fno.adapters.providers.loader import load_combos, load_quota_config
    from fno.adapters.providers.runtime_state import (
        HeadroomState,
        advance_cursor,
        headroom,
        read_cursor,
        read_state,
        update_provider_health,
    )

    combos = load_combos()
    if combo_name not in combos:
        raise ComboNotFoundError(
            f"combo {combo_name!r} not found (known: {sorted(combos)})"
        )
    combo = combos[combo_name]

    # PR #230 review H1: cursor must advance only on served calls so cooldown
    # skips don't burn sticky slots. Read cursor (lock-free) for rotation
    # order; advance only after fn() actually serves a slot (success path or
    # surfaced non-swap-trigger failure - both consume the slot from the
    # rotation's perspective; cooldown-only skips do not).
    if combo.strategy == "round_robin" and len(combo.providers) > 1:
        providers_hash = compute_providers_hash(combo.providers)
        cursor = read_cursor(combo.name, providers_hash)
        idx = (cursor.cursor_index if cursor else 0) % len(combo.providers)
        providers = _rotate_at(combo.providers, idx)
    else:
        providers_hash = None
        providers = list(combo.providers)

    # Quota-aware ordering (x-5d3e): read each member's cached headroom once
    # (no probe - dispatch never adds HTTP latency), then stably demote LOW
    # members below OK/UNKNOWN. UNKNOWN orders WITH OK (Locked Decision 9: no
    # probe is not evidence of trouble). EXHAUSTED members are skipped in the
    # loop below like a cooldown. With no usage snapshots seeded every member
    # is UNKNOWN, so the order and behavior are byte-identical to pre-x-5d3e.
    quota = load_quota_config()
    hr = {
        pid: headroom(
            pid,
            ttl_seconds=quota.probe_ttl_seconds,
            threshold_pct=quota.defer_threshold_pct,
        )
        for pid in providers
    }

    def _rank(pid: str) -> int:
        st = hr[pid].state
        if st is HeadroomState.LOW:
            return 1
        if st is HeadroomState.EXHAUSTED:
            return 2
        return 0  # OK / UNKNOWN

    providers = sorted(providers, key=_rank)  # stable: rotation order kept per rank

    def _maybe_advance() -> None:
        """Advance cursor when a slot was actually served. No-op for non-RR."""
        if providers_hash is not None:
            advance_cursor(
                combo.name,
                sticky_limit=combo.sticky_limit,
                providers_hash=providers_hash,
                providers_count=len(combo.providers),
            )

    last_outcome: CallOutcome | None = None
    soonest_retry: float | None = None

    def _track_retry(candidate: float | None) -> None:
        nonlocal soonest_retry
        if candidate is None:
            return
        if soonest_retry is None or candidate < soonest_retry:
            soonest_retry = candidate

    for provider_id in providers:
        quota_exhausted = hr[provider_id].state is HeadroomState.EXHAUSTED
        # Read state ONCE per iteration and derive the cooldown from it, instead
        # of is_in_cooldown() (one read) + read_state() (a second read of the
        # same file) on the skip branch. Kept per-iteration (not hoisted before
        # the loop) so a cooldown expiring mid-loop - while an earlier fn() ran -
        # is still seen on the next provider (AC3.3). Fail-open on a bad read.
        try:
            health = read_state().provider_health.get(provider_id)
        except Exception:  # noqa: BLE001 - a corrupt state read never wedges dispatch
            health = None
        rlu = health.rate_limited_until if health is not None else None
        in_cooldown = rlu is not None and rlu > time.time()
        if in_cooldown or quota_exhausted:
            # Track the soonest eligibility time for the QueueExhausted hint,
            # from the reactive cooldown (rate_limited_until) AND the predictive
            # quota reset (headroom.resets_at). A cooldown/quota skip does NOT
            # advance the cursor (post-review fix / AC2-EDGE).
            if rlu is not None:
                _track_retry(rlu)
            if quota_exhausted:
                _track_retry(hr[provider_id].resets_at)
            continue
        outcome = fn(provider_id)
        last_outcome = outcome
        if outcome.success:
            record_success(provider_id)
            _maybe_advance()
            return outcome
        if outcome.swap_trigger:
            # Swap-trigger writes provider health (cooldown). Do NOT advance
            # the rotation cursor - the slot was attempted but failed; the
            # next provider in the rotated order takes over.
            rule = classify_error(outcome.status, outcome.body)
            if rule is not None:
                update_provider_health(provider_id, rule)
            continue
        # Non-swap-trigger failure: surface immediately. The provider DID
        # serve the slot (the failure is the caller's problem to handle, not
        # rotation's), so advance the cursor.
        _maybe_advance()
        return outcome

    # Queue exhausted with no served call: do NOT advance.
    return QueueExhausted(last_outcome=last_outcome, retry_after=soonest_retry)
