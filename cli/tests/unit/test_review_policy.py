"""Tests for the pre-review risk classifier and assurance resolution.

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
        identity_known=True,
    )
    assert v.satisfied is True
    assert v.effective == "diverse"


def test_high_assurance_unresolved_when_only_same_family() -> None:
    """quota-exhausted/one-subscription: no different family -> unresolved (blocks)."""
    v = assess_assurance(
        ReviewPolicy.HIGH_ASSURANCE,
        effective_reviewer_kinds=["claude"],
        implementer_provider="claude",
        identity_known=True,
    )
    assert v.satisfied is False
    assert v.effective == "unresolved"


def test_high_assurance_default_identity_fails_closed() -> None:
    """Omitting identity_known must default to unresolved, not a permissive pass."""
    v = assess_assurance(
        ReviewPolicy.HIGH_ASSURANCE,
        effective_reviewer_kinds=["claude", "codex"],
        implementer_provider="claude",
    )
    assert v.satisfied is False
    assert v.effective == "unresolved"


def test_non_dispatchable_kind_does_not_certify_diversity() -> None:
    """A dead/unknown kind (not in DISPATCHABLE_PROVIDERS) is not different-family."""
    v = assess_assurance(
        ReviewPolicy.HIGH_ASSURANCE,
        effective_reviewer_kinds=["claude", "grok"],
        implementer_provider="claude",
        identity_known=True,
    )
    assert v.satisfied is False
    assert v.effective == "unresolved"


def test_uppercase_risk_surface_still_escalates() -> None:
    """Risk surfaces from config/CLI are normalized before matching."""
    assert (
        classify_review_policy(size="S", risk_surfaces=["  AUTH  "])
        is ReviewPolicy.HIGH_ASSURANCE
    )


def test_assurance_verdict_rejects_inconsistent_state() -> None:
    """The frozen verdict enforces satisfied <-> effective correlation."""
    from fno.review.policy import AssuranceVerdict

    with pytest.raises(ValueError):
        AssuranceVerdict(ReviewPolicy.PORTABLE, satisfied=True, effective="unresolved", reason="x")
    with pytest.raises(ValueError):
        AssuranceVerdict(ReviewPolicy.PORTABLE, satisfied=False, effective="portable", reason="x")


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


# ---- fno do review --assess-assurance: the CLI gate exits nonzero when unsatisfied ----


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
            ["do", "review", "--assess-assurance", "--policy-size", "S", "--risk-surface", "merge-gate"],
        )
    assert result.exit_code == 3
    assert '"satisfied": false' in result.stdout


def test_cli_assess_assurance_exits_0_when_satisfied() -> None:
    sat = {"policy": "portable", "satisfied": True, "effective": "portable", "reason": "ok"}
    with patch("fno.worker.review.review_assurance", return_value=sat):
        result = CliRunner().invoke(
            app, ["do", "review", "--assess-assurance", "--policy-size", "S"]
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


def test_review_assurance_unreadable_headroom_fails_closed() -> None:
    """exhausted_provider_kinds -> None (read error) must block high-assurance,
    never let a possibly-exhausted codex certify diversity."""
    with patch("fno.review.provider_resolution.load_implementer_identity", return_value=("claude", True)), patch(
        "fno.review.provider_resolution.exhausted_provider_kinds", return_value=None
    ), patch.object(
        review_mod, "panel_provider_routing", return_value=_routing(a=("codex", False))
    ):
        v = review_mod.review_assurance("sess", size="S", risk_surfaces=["auth"])
    assert v["satisfied"] is False
    assert v["effective"] == "unresolved"
    assert v["headroom_unknown"] is True


# ---- real substrate bodies (F3/F4 live here; the accessor tests above mock them) ----

import json  # noqa: E402

from fno.review import provider_resolution as prov  # noqa: E402


def test_load_implementer_identity_established_on_dispatchable_id(tmp_path) -> None:
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [{"session_id": "s1", "provider_id": "codex"}]}))
    assert prov.load_implementer_identity("s1", ledger_path=ledger) == ("codex", True)


def test_load_implementer_identity_unknown_when_id_unmappable(tmp_path) -> None:
    """F3: a rotated-out id that maps to no real kind is unknown, not known-claude."""
    ledger = tmp_path / "ledger.json"
    ledger.write_text(
        json.dumps({"entries": [{"session_id": "s1", "provider_id": "codex-acct-rotated-out"}]})
    )
    kind, established = prov.load_implementer_identity("s1", ledger_path=ledger)
    assert kind == "claude"
    assert established is False


def test_load_implementer_identity_unknown_without_row(tmp_path) -> None:
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [{"session_id": "other", "provider_id": "codex"}]}))
    assert prov.load_implementer_identity("s1", ledger_path=ledger) == ("claude", False)


def test_load_implementer_identity_no_session_or_file(tmp_path) -> None:
    assert prov.load_implementer_identity("") == ("claude", False)
    assert prov.load_implementer_identity("s1", ledger_path=tmp_path / "absent.json") == (
        "claude",
        False,
    )


def _rec(rid: str, harness: str):
    return SimpleNamespace(id=rid, harness=harness)


def test_exhausted_provider_kinds_all_records_exhausted() -> None:
    from fno.adapters.providers.runtime_state import HeadroomState

    records = SimpleNamespace(records=[_rec("c1", "codex"), _rec("c2", "codex")])
    with patch("fno.adapters.providers.loader.load_providers", return_value=records), patch(
        "fno.adapters.providers.runtime_state.headroom",
        return_value=SimpleNamespace(state=HeadroomState.EXHAUSTED),
    ):
        assert prov.exhausted_provider_kinds() == {"codex"}


def test_exhausted_provider_kinds_not_when_one_record_has_headroom() -> None:
    """F4 semantic: `all`, not `any` - one account with headroom keeps the kind up."""
    from fno.adapters.providers.runtime_state import HeadroomState

    records = SimpleNamespace(records=[_rec("c1", "codex"), _rec("c2", "codex")])

    def _hr(pid):
        return SimpleNamespace(
            state=HeadroomState.EXHAUSTED if pid == "c1" else HeadroomState.UNKNOWN
        )

    with patch("fno.adapters.providers.loader.load_providers", return_value=records), patch(
        "fno.adapters.providers.runtime_state.headroom", side_effect=_hr
    ):
        assert prov.exhausted_provider_kinds() == set()


def test_exhausted_provider_kinds_returns_none_on_read_error() -> None:
    with patch(
        "fno.adapters.providers.loader.load_providers", side_effect=RuntimeError("boom")
    ):
        assert prov.exhausted_provider_kinds() is None
