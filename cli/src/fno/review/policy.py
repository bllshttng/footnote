"""Pre-review risk classification and assurance resolution.

A deterministic layer that sits on top of the capacity substrate in
:mod:`fno.review.provider_resolution`. It answers two questions before a
review runs:

1. *What assurance does this change need?* -> :func:`classify_review_policy`
   maps plan size + named risk surfaces to a :class:`ReviewPolicy`.
2. *Does the capacity we actually have satisfy it?* -> :func:`assess_assurance`
   composes the already-resolved available/implementer providers into a
   :class:`AssuranceVerdict`.

Two load-bearing invariants, both from the epic:

* **Portable baseline.** ``portable`` / ``diverse_preferred`` / ``full_sigma``
  never block on cross-model capacity. One subscription always reviews via
  fresh-context same-family. Different-family capacity is a *preference*, never
  a paywall.
* **High assurance is the only blocker.** ``high_assurance`` leaves the verdict
  UNRESOLVED (not satisfied) when a genuine different-family reviewer cannot be
  identified - unknown implementer identity or no diverse capacity. That is the
  one policy allowed to withhold a passing verdict.

Pure and total: no I/O, never raises for the two functions above. The thin
production accessor that feeds them from the real ledger/provider substrate
(``review_assurance``, below) lives here too since the sigma
panel's removal: it assesses the policy against the reviewer capacity that will
actually run, which after the panel's removal is the lane's own runtime plus
the configured cross-model provider kinds.
"""
from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import Literal, Optional, Sequence

from fno.review.provider_resolution import CLAUDE, DISPATCHABLE_PROVIDERS

Effective = Literal["diverse", "portable", "unresolved"]


class ReviewPolicy(str, Enum):
    """The assurance profile a change is reviewed under."""

    PORTABLE = "portable"
    DIVERSE_PREFERRED = "diverse_preferred"
    FULL_SIGMA = "full_sigma"
    HIGH_ASSURANCE = "high_assurance"


# Risk surfaces that force high-assurance regardless of size. These are the
# change kinds where a same-family self-review is not enough: the gates and
# secrets that guard the pipeline itself, plus data/money surfaces.
HIGH_ASSURANCE_SURFACES: frozenset[str] = frozenset(
    {
        "auth",
        "security",
        "secrets",
        "merge-gate",
        "review-gate",
        "loopcheck",
        "migration",
        "money",
        "payments",
    }
)


def classify_review_policy(
    *, size: str | None, risk_surfaces: Sequence[str] | None = None
) -> ReviewPolicy:
    """Select the review policy from plan ``size`` and named ``risk_surfaces``.

    Deterministic and total (unknown size -> ``PORTABLE``):

    * any named high-assurance surface  -> ``HIGH_ASSURANCE`` (surfaces win over size)
    * size ``L``                        -> ``FULL_SIGMA``
    * size ``M``                        -> ``DIVERSE_PREFERRED``
    * size ``S`` / unknown              -> ``PORTABLE``
    """
    surfaces = {s.strip().lower() for s in (risk_surfaces or []) if s and s.strip()}
    if surfaces & HIGH_ASSURANCE_SURFACES:
        return ReviewPolicy.HIGH_ASSURANCE
    normalized = (size or "").strip().upper()
    if normalized == "L":
        return ReviewPolicy.FULL_SIGMA
    if normalized == "M":
        return ReviewPolicy.DIVERSE_PREFERRED
    return ReviewPolicy.PORTABLE


@dataclass(frozen=True)
class AssuranceVerdict:
    """Whether the achieved capacity satisfies the declared policy.

    ``effective`` is ``diverse`` when a genuine different-family reviewer is
    available, ``portable`` when we fall back to same-family fresh-context, and
    ``unresolved`` when a high-assurance requirement cannot be met.
    """

    policy: ReviewPolicy
    satisfied: bool
    effective: Effective
    reason: str

    def __post_init__(self) -> None:
        # satisfied and effective are correlated, not independent: unresolved iff
        # not satisfied. Enforce it at construction so a future branch in
        # assess_assurance that mixes them fails here, not silently downstream.
        if (self.effective == "unresolved") == self.satisfied:
            raise ValueError(
                f"inconsistent verdict: satisfied={self.satisfied} "
                f"effective={self.effective!r}"
            )


def _has_diverse_capacity(
    effective_reviewer_kinds: Sequence[str], implementer_provider: str
) -> bool:
    """True when a reviewer RESOLVED TO DISPATCH differs from the implementer.

    ``effective_reviewer_kinds`` is what the panel is resolved to dispatch to -
    already excluding degraded fallbacks and exhausted providers (the accessor
    computes it from the resolved routing, not from raw capacity). This is the
    fix for the "certifies unused capacity" failure: a codex record that exists
    but is disabled, pinned away, or exhausted is NOT in this set, so it cannot
    make a high-assurance verdict pass. This is an AVAILABILITY signal (preflight):
    it does not prove the reviewer later completed cross-family - a dispatch that
    times out and falls back is caught post-hoc by the observed-runtime
    attestation, not here.
    """
    implementer = (implementer_provider or CLAUDE).strip().lower()
    for candidate in effective_reviewer_kinds:
        cand = str(candidate).strip().lower()
        if cand in DISPATCHABLE_PROVIDERS and cand != implementer:
            return True
    return False


def assess_assurance(
    policy: ReviewPolicy,
    *,
    effective_reviewer_kinds: Sequence[str],
    implementer_provider: str,
    identity_known: bool = False,
) -> AssuranceVerdict:
    """Resolve ``policy`` against the reviewer resolved to dispatch. Never raises.

    This is a PREFLIGHT availability verdict, not a proof that a cross-family
    review completed; the observed-runtime attestation gates completion.

    ``effective_reviewer_kinds`` is the set of provider kinds a panel agent is
    resolved to dispatch to for this run - degraded fallbacks collapsed to claude
    and exhausted kinds removed - NOT the raw capacity list. ``implementer_provider``
    is the kind that wrote the code; ``identity_known`` is False when that family
    could not be established from real provenance (no ledger row), which only
    high-assurance treats as blocking.
    """
    diverse = _has_diverse_capacity(effective_reviewer_kinds, implementer_provider)

    if policy is ReviewPolicy.HIGH_ASSURANCE:
        if not identity_known:
            return AssuranceVerdict(
                policy,
                satisfied=False,
                effective="unresolved",
                reason="high-assurance requires a known implementer family; identity unknown",
            )
        if not diverse:
            return AssuranceVerdict(
                policy,
                satisfied=False,
                effective="unresolved",
                reason="high-assurance requires different-family capacity; none available",
            )
        return AssuranceVerdict(
            policy,
            satisfied=True,
            effective="diverse",
            reason="different-family reviewer available",
        )

    # portable / diverse_preferred / full_sigma: never block on capacity. Prefer
    # diverse when present, otherwise same-family fresh-context is acceptable.
    if diverse:
        return AssuranceVerdict(
            policy, satisfied=True, effective="diverse", reason="different-family reviewer available"
        )
    return AssuranceVerdict(
        policy,
        satisfied=True,
        effective="portable",
        reason="same-family fresh-context review (no different-family capacity)",
    )


# ── production accessors (moved here from the deleted panel worker) ──


def _read_state(state_path: Path) -> dict:
    """The target manifest's five-field read contract, tolerating absence."""
    import json

    try:
        raw = json.loads(state_path.read_text(encoding="utf-8"))
        return raw if isinstance(raw, dict) else {}
    except Exception:  # noqa: BLE001 - manifest is optional for these reads
        return {}


def _cross_model_enabled() -> bool:
    """``config.review.cross_model.enabled`` - the diversity switch.

    Fail-open on an unreadable settings load (the substrate decides capacity;
    this switch only decides whether non-claude kinds MAY serve), matching the
    cross-model gates' own read direction.
    """
    try:
        from fno.config import load_settings

        return bool(load_settings().review.cross_model.enabled)
    except Exception:  # noqa: BLE001 - unreadable config never narrows capacity
        return True


def resolve_session_id(session_id: Optional[str], state_path: Path) -> Optional[str]:
    """Resolve the session nonce: explicit arg, else the state file's value.

    The ONE place assurance resolution reads the session, so every surface
    that names the implementer provider resolves identically. ``None`` when
    neither source has it; callers decide whether that is fatal.
    """
    if session_id:
        return session_id
    return _read_state(state_path).get("session_id")


def review_assurance(
    session_id: Optional[str],
    *,
    size: Optional[str],
    risk_surfaces: Optional[list[str]] = None,
    state_path: Optional[Path] = None,
) -> dict:
    """Assess the review policy against the reviewer that will ACTUALLY run.

    - implementer family + whether it was established come from real ledger
      provenance (``load_implementer_identity``); a session id with no ledger
      row is *unknown*, not defaulted-claude.
    - the effective reviewer kinds are the local runtime (always claude) plus
      every provider kind the capacity substrate can genuinely serve TODAY
      while config.review.cross_model is enabled: exhausted kinds are removed,
      a disabled cross-model switch yields claude alone (F2), and an
      unreadable headroom read fails CLOSED - only claude counts when
      headroom cannot be measured, because treating a read error as "nothing
      exhausted" reopens the diversity hole.

    Never raises.
    """
    from fno.review import provider_resolution as pr

    implementer, identity_known = pr.load_implementer_identity(session_id or "")

    # exhausted_provider_kinds returns None when the headroom read FAILED. For
    # this gate an unreadable headroom fails CLOSED: we cannot trust a
    # non-claude kind can actually serve, so it does not count toward
    # diversity.
    exhausted = pr.exhausted_provider_kinds()
    headroom_unknown = exhausted is None
    exhausted = exhausted or set()

    effective_kinds: set[str] = {CLAUDE}  # local runtime is always effective
    if _cross_model_enabled():
        for kind in pr.available_provider_kinds():
            kind = str(kind).strip().lower()
            if kind in exhausted or headroom_unknown:
                if kind != CLAUDE:
                    continue
            effective_kinds.add(kind)

    policy = classify_review_policy(size=size, risk_surfaces=risk_surfaces)
    verdict = assess_assurance(
        policy,
        effective_reviewer_kinds=sorted(effective_kinds),
        implementer_provider=implementer,
        identity_known=identity_known,
    )
    return {
        "policy": verdict.policy.value,
        "satisfied": verdict.satisfied,
        "effective": verdict.effective,
        "reason": verdict.reason,
        "implementer_provider": implementer,
        "identity_known": identity_known,
        "effective_reviewer_kinds": sorted(effective_kinds),
        "exhausted_kinds": sorted(exhausted),
        "headroom_unknown": headroom_unknown,
    }
