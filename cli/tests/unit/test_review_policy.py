"""Tests for the pre-review risk classifier and assurance resolution (x-5f0c).

Covers the two pure functions in ``fno.review.policy``:
``classify_review_policy`` (size + risk -> policy) and ``assess_assurance``
(policy + capacity -> verdict), driving the epic's fixture matrix:
one-subscription, different-family-available, quota-exhausted, unknown-identity,
and high-assurance-unsatisfied.
"""
from __future__ import annotations

import pytest

from typer.testing import CliRunner
from unittest.mock import patch

from fno.cli import app
from fno.review.policy import (
    ReviewPolicy,
    assess_assurance,
    classify_review_policy,
)


# ---- classify_review_policy: size + risk -> policy ----


@pytest.mark.parametrize(
    "size,expected",
    [
        ("L", ReviewPolicy.FULL_SIGMA),
        ("l", ReviewPolicy.FULL_SIGMA),
        ("M", ReviewPolicy.DIVERSE_PREFERRED),
        ("S", ReviewPolicy.PORTABLE),
        ("", ReviewPolicy.PORTABLE),
        (None, ReviewPolicy.PORTABLE),
    ],
)
def test_classify_by_size(size, expected) -> None:
    assert classify_review_policy(size=size) is expected


def test_risk_surface_forces_high_assurance_over_size() -> None:
    """A named high-assurance surface wins even for a small change."""
    assert (
        classify_review_policy(size="S", risk_surfaces=["merge-gate"])
        is ReviewPolicy.HIGH_ASSURANCE
    )
    assert (
        classify_review_policy(size="L", risk_surfaces=["auth"])
        is ReviewPolicy.HIGH_ASSURANCE
    )


def test_unknown_risk_surface_does_not_escalate() -> None:
    """A surface not in the high-assurance set leaves size-based classification."""
    assert (
        classify_review_policy(size="M", risk_surfaces=["ui", "docs"])
        is ReviewPolicy.DIVERSE_PREFERRED
    )


# ---- assess_assurance: portable baseline never blocks ----


def test_one_subscription_portable_is_satisfied() -> None:
    """One subscription (claude-only) reviews via same-family fresh-context."""
    v = assess_assurance(
        ReviewPolicy.PORTABLE,
        effective_reviewer_kinds=["claude"],
        implementer_provider="claude",
    )
    assert v.satisfied is True
    assert v.effective == "portable"


def test_full_sigma_with_one_subscription_still_satisfied() -> None:
    """Anti-paywall: a large change on one subscription is not blocked."""
    v = assess_assurance(
        ReviewPolicy.FULL_SIGMA,
        effective_reviewer_kinds=["claude"],
        implementer_provider="claude",
    )
    assert v.satisfied is True
    assert v.effective == "portable"


def test_diverse_preferred_uses_different_family_when_available() -> None:
    v = assess_assurance(
        ReviewPolicy.DIVERSE_PREFERRED,
        effective_reviewer_kinds=["claude", "codex"],
        implementer_provider="claude",
    )
    assert v.satisfied is True
    assert v.effective == "diverse"


def test_diverse_preferred_degrades_to_portable_without_capacity() -> None:
    v = assess_assurance(
        ReviewPolicy.DIVERSE_PREFERRED,
        effective_reviewer_kinds=["claude"],
        implementer_provider="claude",
    )
    assert v.satisfied is True
    assert v.effective == "portable"


# ---- assess_assurance: high-assurance is the only blocker ----


def test_high_assurance_satisfied_with_different_family() -> None:
    v = assess_assurance(
        ReviewPolicy.HIGH_ASSURANCE,
        effective_reviewer_kinds=["claude", "codex"],
        implementer_provider="claude",
    )
    assert v.satisfied is True
    assert v.effective == "diverse"


def test_high_assurance_unresolved_when_only_same_family() -> None:
    """quota-exhausted/one-subscription: no different family -> unresolved (blocks)."""
    v = assess_assurance(
        ReviewPolicy.HIGH_ASSURANCE,
        effective_reviewer_kinds=["claude"],
        implementer_provider="claude",
    )
    assert v.satisfied is False
    assert v.effective == "unresolved"


def test_high_assurance_unresolved_when_identity_unknown() -> None:
    """Unknown implementer family -> cannot guarantee diversity -> unresolved."""
    v = assess_assurance(
        ReviewPolicy.HIGH_ASSURANCE,
        effective_reviewer_kinds=["claude", "codex"],
        implementer_provider="claude",
        identity_known=False,
    )
    assert v.satisfied is False
    assert v.effective == "unresolved"


def test_quota_exhausted_alternate_falls_back_for_soft_policy() -> None:
    """quota-exhausted: the only different family (codex) is exhausted so it is
    absent from availability; a soft policy degrades to portable, not blocked."""
    # available_provider_kinds already demotes/removes exhausted kinds, so an
    # exhausted codex simply is not in the list the caller passes here.
    v = assess_assurance(
        ReviewPolicy.DIVERSE_PREFERRED,
        effective_reviewer_kinds=["claude"],
        implementer_provider="claude",
    )
    assert v.satisfied is True
    assert v.effective == "portable"


# ---- fno review --assess-assurance: the CLI gate exits nonzero when unsatisfied ----


def test_cli_assess_assurance_exits_3_when_unsatisfied() -> None:
    """A direct CLI caller gets a nonzero exit, not a clean pass, when a
    high-assurance change cannot be reviewed cross-family."""
    unsat = {
        "policy": "high_assurance",
        "satisfied": False,
        "effective": "unresolved",
        "reason": "no different-family capacity",
    }
    with patch("fno.worker.review.review_assurance", return_value=unsat):
        result = CliRunner().invoke(
            app,
            ["review", "--assess-assurance", "--policy-size", "S", "--risk-surface", "merge-gate"],
        )
    assert result.exit_code == 3
    assert '"satisfied": false' in result.stdout


def test_cli_assess_assurance_exits_0_when_satisfied() -> None:
    sat = {"policy": "portable", "satisfied": True, "effective": "portable", "reason": "ok"}
    with patch("fno.worker.review.review_assurance", return_value=sat):
        result = CliRunner().invoke(
            app, ["review", "--assess-assurance", "--policy-size", "S"]
        )
    assert result.exit_code == 0


# ---- review_assurance accessor: assess the reviewer that will ACTUALLY run ----
#
# These lock the four review findings: the gate must not certify unused capacity
# (disabled cross-model, exhausted quota) or a fabricated implementer identity.

from types import SimpleNamespace  # noqa: E402

from fno.worker import review as review_mod  # noqa: E402


def _routing(**kinds) -> dict:
    """A resolved-panel map: agent -> ResolvedProvider-like (provider, degraded)."""
    return {
        agent: SimpleNamespace(provider=prov, degraded=deg)
        for agent, (prov, deg) in kinds.items()
    }


def test_review_assurance_high_assurance_passes_with_real_codex_reviewer() -> None:
    with patch("fno.review.provider_resolution.load_implementer_identity", return_value=("claude", True)), patch(
        "fno.review.provider_resolution.exhausted_provider_kinds", return_value=set()
    ), patch.object(
        review_mod, "panel_provider_routing", return_value=_routing(a=("codex", False))
    ):
        v = review_mod.review_assurance("sess", size="S", risk_surfaces=["merge-gate"])
    assert v["satisfied"] is True
    assert v["effective"] == "diverse"


def test_review_assurance_blocks_when_cross_model_disabled() -> None:
    """F2: a codex record exists but cross-model is off -> routing is all-claude."""
    with patch("fno.review.provider_resolution.load_implementer_identity", return_value=("claude", True)), patch(
        "fno.review.provider_resolution.exhausted_provider_kinds", return_value=set()
    ), patch.object(review_mod, "panel_provider_routing", return_value={}):
        v = review_mod.review_assurance("sess", size="S", risk_surfaces=["auth"])
    assert v["satisfied"] is False
    assert v["effective"] == "unresolved"


def test_review_assurance_blocks_on_unknown_identity() -> None:
    """F3: session id present but no ledger row -> family unestablished."""
    with patch("fno.review.provider_resolution.load_implementer_identity", return_value=("claude", False)), patch(
        "fno.review.provider_resolution.exhausted_provider_kinds", return_value=set()
    ), patch.object(
        review_mod, "panel_provider_routing", return_value=_routing(a=("codex", False))
    ):
        v = review_mod.review_assurance("sess", size="S", risk_surfaces=["migration"])
    assert v["satisfied"] is False
    assert v["effective"] == "unresolved"


def test_review_assurance_blocks_when_diverse_kind_exhausted() -> None:
    """F4: the only different family is out of quota -> not effective capacity."""
    with patch("fno.review.provider_resolution.load_implementer_identity", return_value=("claude", True)), patch(
        "fno.review.provider_resolution.exhausted_provider_kinds", return_value={"codex"}
    ), patch.object(
        review_mod, "panel_provider_routing", return_value=_routing(a=("codex", False))
    ):
        v = review_mod.review_assurance("sess", size="S", risk_surfaces=["secrets"])
    assert v["satisfied"] is False
    assert v["effective"] == "unresolved"
    assert v["exhausted_kinds"] == ["codex"]


def test_review_assurance_degraded_route_does_not_count_as_diverse() -> None:
    """A degraded resolution ran on claude, so it is not different-family."""
    with patch("fno.review.provider_resolution.load_implementer_identity", return_value=("claude", True)), patch(
        "fno.review.provider_resolution.exhausted_provider_kinds", return_value=set()
    ), patch.object(
        review_mod, "panel_provider_routing", return_value=_routing(a=("claude", True))
    ):
        v = review_mod.review_assurance("sess", size="S", risk_surfaces=["payments"])
    assert v["satisfied"] is False
    assert v["effective"] == "unresolved"
