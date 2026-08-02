from __future__ import annotations

from collections.abc import Callable
from datetime import datetime, timedelta, timezone

import pytest
from pydantic import ValidationError

from fno.company.contracts import (
    CompanyWorkRefs,
    DeliverableRef,
    EvidenceRef,
    EvidenceResult,
    EvidenceSubjectKind,
    FunctionRef,
    WorkOrderRef,
)
from fno.delivery import (
    DELIVERY_EVALUATOR_VERSION,
    DELIVERY_EVIDENCE_FACT_VERSION,
    DeliveryEvidenceFact,
    evaluate_delivery,
)

NOW = datetime(2026, 8, 2, 12, tzinfo=timezone.utc)
NODE_ID = "x-delivery"
ATTEMPT_ID = "attempt-1"
REQUIREMENTS = (
    ("artifact-ready", EvidenceSubjectKind.ARTIFACT, "artifact-1"),
    ("review-ready", EvidenceSubjectKind.REVIEW, "review-1"),
    ("approval-ready", EvidenceSubjectKind.APPROVAL, "approval-1"),
    ("probe-ready", EvidenceSubjectKind.PROBE, "probe-1"),
    ("ack-ready", EvidenceSubjectKind.ACKNOWLEDGMENT, "ack-1"),
)


def _company_work(function_name: str = "engineering") -> CompanyWorkRefs:
    return CompanyWorkRefs(
        function=FunctionRef(id=function_name),
        work_order=WorkOrderRef(node_id=NODE_ID, attempt_id=ATTEMPT_ID),
        deliverables=(
            DeliverableRef(
                id="release",
                kind="arbitrary-deliverable",
                work_order_id=NODE_ID,
                attempt_id=ATTEMPT_ID,
                required_evidence_ids=tuple(item[0] for item in REQUIREMENTS),
            ),
        ),
        evidence=tuple(
            EvidenceRef(
                id=evidence_id,
                work_order_id=NODE_ID,
                attempt_id=ATTEMPT_ID,
                subject_kind=subject_kind,
                subject_id=subject_id,
                result=EvidenceResult.PASSED,
            )
            for evidence_id, subject_kind, subject_id in REQUIREMENTS
        ),
    )


def _fact(
    evidence_id: str,
    *,
    result: EvidenceResult = EvidenceResult.PASSED,
    producer: str = "adapter:test",
    observed_at: datetime = NOW,
    fresh_until: datetime | None = None,
    attempt_id: str = ATTEMPT_ID,
    subject_kind: EvidenceSubjectKind | None = None,
    subject_id: str | None = None,
    fact_revision: str = "snapshot-7",
) -> DeliveryEvidenceFact:
    requirement = next(item for item in REQUIREMENTS if item[0] == evidence_id)
    return DeliveryEvidenceFact(
        version=DELIVERY_EVIDENCE_FACT_VERSION,
        evidence=EvidenceRef(
            id=evidence_id,
            work_order_id=NODE_ID,
            attempt_id=attempt_id,
            subject_kind=subject_kind or requirement[1],
            subject_id=subject_id or requirement[2],
            result=result,
        ),
        producer=producer,
        observed_at=observed_at,
        source_revision="source-sha-1",
        fresh_until=fresh_until or NOW + timedelta(minutes=5),
        adapter_version="test-adapter.v1",
        fact_revision=fact_revision,
    )


def _facts() -> tuple[DeliveryEvidenceFact, ...]:
    return tuple(_fact(item[0]) for item in REQUIREMENTS)


def test_ac_d1_hp_complete_five_slot_coverage_passes_in_declaration_order() -> None:
    verdict = evaluate_delivery(_company_work(), _facts(), evaluated_at=NOW)

    assert verdict.evaluator_version == DELIVERY_EVALUATOR_VERSION
    assert verdict.work_order_node_id == NODE_ID
    assert verdict.attempt_id == ATTEMPT_ID
    assert verdict.fact_revision == "snapshot-7"
    assert verdict.aggregate is EvidenceResult.PASSED
    assert [row.evidence_id for row in verdict.requirements] == [
        item[0] for item in REQUIREMENTS
    ]
    assert {row.result for row in verdict.requirements} == {EvidenceResult.PASSED}


def test_ac_d1_err_projection_only_passed_values_are_not_runtime_evidence() -> None:
    verdict = evaluate_delivery(_company_work(), (), evaluated_at=NOW)

    assert verdict.aggregate is EvidenceResult.UNKNOWN
    assert all(row.result is EvidenceResult.UNKNOWN for row in verdict.requirements)
    assert "artifact-ready" in verdict.requirements[0].diagnostics[0]
    assert "missing runtime fact" in verdict.requirements[0].diagnostics[0]


@pytest.mark.parametrize(
    ("facts", "evidence_id", "diagnostic"),
    [
        (lambda: _facts()[1:], "artifact-ready", "missing runtime fact"),
        (
            lambda: (
                _fact(
                    "artifact-ready",
                    observed_at=NOW - timedelta(minutes=10),
                    fresh_until=NOW - timedelta(seconds=1),
                ),
            )
            + _facts()[1:],
            "artifact-ready",
            "stale",
        ),
        (
            lambda: (_fact("artifact-ready", attempt_id="attempt-old"),) + _facts()[1:],
            "artifact-ready",
            "attempt-old",
        ),
        (
            lambda: (
                _fact("artifact-ready", producer="adapter:a"),
                _fact(
                    "artifact-ready",
                    producer="adapter:b",
                    result=EvidenceResult.FAILED,
                ),
            )
            + _facts()[1:],
            "artifact-ready",
            "adapter:a",
        ),
    ],
)
def test_ac_d2_err_missing_stale_mismatched_and_conflicting_facts_are_unknown(
    facts: Callable[[], tuple[DeliveryEvidenceFact, ...]],
    evidence_id: str,
    diagnostic: str,
) -> None:
    verdict = evaluate_delivery(_company_work(), facts(), evaluated_at=NOW)
    row = next(item for item in verdict.requirements if item.evidence_id == evidence_id)

    assert row.result is EvidenceResult.UNKNOWN
    assert verdict.aggregate is EvidenceResult.UNKNOWN
    assert evidence_id in " ".join(row.diagnostics)
    assert diagnostic in " ".join(row.diagnostics)


def test_ac_d2_err_malformed_and_unknown_version_facts_name_the_source() -> None:
    malformed = _fact("artifact-ready").model_dump(mode="json")
    malformed["version"] = "delivery-evidence-fact.v999"
    malformed["producer"] = "adapter:future"

    verdict = evaluate_delivery(
        _company_work(), (malformed,) + _facts()[1:], evaluated_at=NOW
    )

    row = verdict.requirements[0]
    assert row.result is EvidenceResult.UNKNOWN
    assert "artifact-ready" in " ".join(row.diagnostics)
    assert "adapter:future" in " ".join(row.diagnostics)
    assert "unknown version" in " ".join(row.diagnostics)


def test_ac_d3_err_precedence_is_failed_then_blocked_then_unknown_with_all_rows() -> None:
    facts = (
        _fact("artifact-ready", result=EvidenceResult.FAILED),
        _fact("review-ready", result=EvidenceResult.BLOCKED),
    )

    verdict = evaluate_delivery(_company_work(), facts, evaluated_at=NOW)

    assert verdict.aggregate is EvidenceResult.FAILED
    assert [row.result for row in verdict.requirements] == [
        EvidenceResult.FAILED,
        EvidenceResult.BLOCKED,
        EvidenceResult.UNKNOWN,
        EvidenceResult.UNKNOWN,
        EvidenceResult.UNKNOWN,
    ]


def test_ac_d8_inv_observation_facts_are_excluded_and_irrelevant() -> None:
    passing = evaluate_delivery(_company_work(), _facts(), evaluated_at=NOW)
    observation = _fact(
        "artifact-ready",
        result=EvidenceResult.FAILED,
        producer="adapter:observation",
        subject_kind=EvidenceSubjectKind.OBSERVATION,
        subject_id="later-business-metric",
    )
    with_observation = evaluate_delivery(
        _company_work(), _facts() + (observation,), evaluated_at=NOW
    )

    assert passing == with_observation
    assert with_observation.aggregate is EvidenceResult.PASSED


@pytest.mark.parametrize(
    "function_name",
    [
        "marketing",
        "communications",
        "design",
        "social",
        "support",
        "operations",
        "sales",
        "arbitrary-unknown-function",
    ],
)
def test_ac_d9_inv_evaluation_is_function_agnostic(function_name: str) -> None:
    verdict = evaluate_delivery(_company_work(function_name), _facts(), evaluated_at=NOW)

    assert verdict.aggregate is EvidenceResult.PASSED
    assert len(verdict.requirements) == len(REQUIREMENTS)


def test_ac_d10_con_mixed_fact_revisions_fail_closed() -> None:
    facts = (_fact("artifact-ready", fact_revision="snapshot-6"),) + _facts()[1:]

    verdict = evaluate_delivery(_company_work(), facts, evaluated_at=NOW)

    assert verdict.aggregate is EvidenceResult.UNKNOWN
    assert verdict.fact_revision is None
    assert all(row.result is EvidenceResult.UNKNOWN for row in verdict.requirements)
    assert "snapshot-6" in " ".join(verdict.diagnostics)
    assert "snapshot-7" in " ".join(verdict.diagnostics)


def test_contracts_are_immutable_and_versions_are_strict() -> None:
    fact = _fact("artifact-ready")

    with pytest.raises(ValidationError, match="frozen"):
        fact.producer = "adapter:other"
    with pytest.raises(ValidationError):
        DeliveryEvidenceFact(**{**fact.model_dump(), "version": "v2"})


def test_ac_d4_compat_pr_shadow_passes_only_with_all_five_legacy_reads() -> None:
    from fno.delivery import LegacyPRSnapshot, adapt_legacy_pr

    snapshot = LegacyPRSnapshot(
        work_order_node_id=NODE_ID,
        attempt_id=ATTEMPT_ID,
        pr_open=True,
        ci_ok=True,
        ci_pending=False,
        reviewed=True,
        head_shipped=True,
        probes_passed=True,
        current_head="head-sha",
        observed_at=NOW,
        fresh_until=NOW + timedelta(minutes=5),
        source_revision="pr-42:head-sha",
        fact_revision="pr-snapshot-1",
    )

    shadow = adapt_legacy_pr(snapshot, evaluated_at=NOW)

    assert shadow.legacy_passed is True
    assert shadow.verdict.aggregate is EvidenceResult.PASSED
    assert shadow.company_work.function is None
    assert [row.evidence_id for row in shadow.verdict.requirements] == [
        "legacy-pr-open",
        "legacy-pr-ci",
        "legacy-pr-review",
        "legacy-pr-head",
        "legacy-pr-probes",
    ]
    assert len(shadow.facts) == 5
    assert {fact.producer for fact in shadow.facts} == {"adapter:legacy-pr"}
    assert {fact.source_revision for fact in shadow.facts} == {"pr-42:head-sha"}
    assert {fact.fact_revision for fact in shadow.facts} == {"pr-snapshot-1"}
    assert {fact.observed_at for fact in shadow.facts} == {NOW}
    assert {fact.fresh_until for fact in shadow.facts} == {
        NOW + timedelta(minutes=5)
    }


@pytest.mark.parametrize(
    ("changes", "evidence_id", "expected"),
    [
        ({"pr_open": False}, "legacy-pr-open", EvidenceResult.FAILED),
        ({"pr_open": None}, "legacy-pr-open", EvidenceResult.UNKNOWN),
        ({"ci_ok": False}, "legacy-pr-ci", EvidenceResult.FAILED),
        ({"ci_ok": False, "ci_pending": True}, "legacy-pr-ci", EvidenceResult.BLOCKED),
        ({"ci_ok": None}, "legacy-pr-ci", EvidenceResult.UNKNOWN),
        ({"reviewed": False}, "legacy-pr-review", EvidenceResult.BLOCKED),
        ({"reviewed": None}, "legacy-pr-review", EvidenceResult.UNKNOWN),
        ({"head_shipped": False}, "legacy-pr-head", EvidenceResult.FAILED),
        ({"head_shipped": None}, "legacy-pr-head", EvidenceResult.UNKNOWN),
        ({"probes_passed": False}, "legacy-pr-probes", EvidenceResult.FAILED),
        ({"probes_passed": None}, "legacy-pr-probes", EvidenceResult.UNKNOWN),
    ],
)
def test_ac_d4_compat_pr_shadow_retains_each_legacy_blocker(
    changes: dict[str, bool | None], evidence_id: str, expected: EvidenceResult
) -> None:
    from fno.delivery import LegacyPRSnapshot, adapt_legacy_pr

    values: dict[str, object] = {
        "work_order_node_id": NODE_ID,
        "attempt_id": ATTEMPT_ID,
        "pr_open": True,
        "ci_ok": True,
        "ci_pending": False,
        "reviewed": True,
        "head_shipped": True,
        "probes_passed": True,
        "current_head": "head-sha",
        "observed_at": NOW,
        "fresh_until": NOW + timedelta(minutes=5),
        "source_revision": "pr-42:head-sha",
        "fact_revision": "pr-snapshot-1",
    }
    values.update(changes)

    shadow = adapt_legacy_pr(LegacyPRSnapshot(**values), evaluated_at=NOW)
    row = next(row for row in shadow.verdict.requirements if row.evidence_id == evidence_id)

    assert shadow.legacy_passed is False
    assert shadow.verdict.aggregate is not EvidenceResult.PASSED
    assert row.result is expected


def test_ac_d4_compat_research_shadow_preserves_exact_grade_and_setup_reads() -> None:
    from fno.delivery import LegacyResearchSnapshot, adapt_legacy_research

    snapshot = LegacyResearchSnapshot(
        work_order_node_id=NODE_ID,
        attempt_id=ATTEMPT_ID,
        uncited_claims=0,
        dead_urls=0,
        sections_uncovered=(),
        artifact_read=True,
        sidecar_read=True,
        observed_at=NOW,
        fresh_until=NOW + timedelta(minutes=5),
        source_revision="brief-sha:sidecar-sha",
        fact_revision="research-snapshot-1",
    )

    shadow = adapt_legacy_research(snapshot, evaluated_at=NOW)

    assert shadow.legacy_passed is True
    assert shadow.verdict.aggregate is EvidenceResult.PASSED
    assert [row.evidence_id for row in shadow.verdict.requirements] == [
        "legacy-research-artifact-read",
        "legacy-research-sidecar-read",
        "legacy-research-uncited-claims",
        "legacy-research-dead-urls",
        "legacy-research-section-coverage",
    ]


@pytest.mark.parametrize(
    ("changes", "evidence_id", "expected"),
    [
        ({"uncited_claims": 1}, "legacy-research-uncited-claims", EvidenceResult.FAILED),
        ({"dead_urls": 1}, "legacy-research-dead-urls", EvidenceResult.FAILED),
        (
            {"sections_uncovered": ("Findings",)},
            "legacy-research-section-coverage",
            EvidenceResult.FAILED,
        ),
        ({"artifact_read": False}, "legacy-research-artifact-read", EvidenceResult.FAILED),
        ({"artifact_read": None}, "legacy-research-artifact-read", EvidenceResult.UNKNOWN),
        ({"sidecar_read": False}, "legacy-research-sidecar-read", EvidenceResult.FAILED),
        ({"sidecar_read": None}, "legacy-research-sidecar-read", EvidenceResult.UNKNOWN),
    ],
)
def test_ac_d4_compat_research_shadow_retains_grade_or_setup_failure(
    changes: dict[str, object], evidence_id: str, expected: EvidenceResult
) -> None:
    from fno.delivery import LegacyResearchSnapshot, adapt_legacy_research

    values: dict[str, object] = {
        "work_order_node_id": NODE_ID,
        "attempt_id": ATTEMPT_ID,
        "uncited_claims": 0,
        "dead_urls": 0,
        "sections_uncovered": (),
        "artifact_read": True,
        "sidecar_read": True,
        "observed_at": NOW,
        "fresh_until": NOW + timedelta(minutes=5),
        "source_revision": "brief-sha:sidecar-sha",
        "fact_revision": "research-snapshot-1",
    }
    values.update(changes)

    shadow = adapt_legacy_research(LegacyResearchSnapshot(**values), evaluated_at=NOW)
    row = next(row for row in shadow.verdict.requirements if row.evidence_id == evidence_id)

    assert shadow.legacy_passed is False
    assert shadow.verdict.aggregate is not EvidenceResult.PASSED
    assert row.result is expected


def test_ac_d4_compat_legacy_snapshots_and_shadow_results_are_immutable() -> None:
    from fno.delivery import LegacyPRSnapshot, adapt_legacy_pr

    snapshot = LegacyPRSnapshot(
        work_order_node_id=NODE_ID,
        attempt_id=ATTEMPT_ID,
        pr_open=True,
        ci_ok=True,
        ci_pending=False,
        reviewed=True,
        head_shipped=True,
        probes_passed=True,
        current_head="head-sha",
        observed_at=NOW,
        fresh_until=NOW + timedelta(minutes=5),
        source_revision="pr-42:head-sha",
        fact_revision="pr-snapshot-1",
    )
    shadow = adapt_legacy_pr(snapshot, evaluated_at=NOW)

    with pytest.raises(ValidationError, match="frozen"):
        snapshot.pr_open = False
    with pytest.raises(ValidationError, match="frozen"):
        shadow.legacy_passed = False
