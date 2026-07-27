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

Pure and total: no I/O, never raises. The thin production accessor that feeds
these from the real ledger/provider substrate lives in
``fno.worker.review.review_assurance``.
"""
from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import Literal, Sequence

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
