"""Context-to-outcome trace contracts for x-2e3c Task 1.2."""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
from copy import deepcopy
from datetime import datetime, timedelta
from pathlib import Path

import pytest

from fno.events.log import normalize_event, read_events
from fno.scoreboard.fold import (
    _comparison_contract_from_events,
    _num_opt,
    build_context_outcome_trace,
    build_plan_fidelity,
    evaluate_context_comparison,
    read_jsonl_events_with_coverage,
    render_context_trace_field_docs,
)


ROOT = Path(__file__).resolve().parents[3]
NOW = datetime(2026, 7, 3, 20, 0, 0)


def _snapshot_event(session_id: str, context_bytes: int, ts: str) -> dict:
    source_hash = f"source-{context_bytes}"
    return {
        "ts": ts,
        "type": "context_snapshot",
        "source": "hook",
        "data": {
            "session_id": session_id,
            "harness": "codex",
            "entry_state": "startup",
            "context_bytes": context_bytes,
            "estimated_tokens": (context_bytes + 3) // 4,
            "context_hash": hashlib.sha256(source_hash.encode()).hexdigest(),
            "source_hashes": [source_hash],
            "source_manifest": [
                {
                    "source_id": "fixture",
                    "status": "observed",
                    "bytes": context_bytes,
                    "content_hash": source_hash,
                }
            ],
            "measurement_complete": True,
            "measurement_errors": [],
        },
    }


def test_event_normalizer_preserves_canonical_and_legacy_identity(
    tmp_path: Path,
) -> None:
    canonical = {
        "ts": "2026-07-03T10:00:00Z",
        "type": "loop_check",
        "source": "hook",
        "data": {
            "session_id": "session-a",
            "node_id": "x-1",
            "pr_number": 42,
            "ci": "SUCCESS",
        },
    }
    legacy = {
        "ts": "2026-07-03T10:01:00Z",
        "type": "context_census",
        "session_id": "session-a",
        "payload": {"graph_node_id": "x-1", "pr_number": "42"},
    }

    assert normalize_event(canonical) == {
        "type": "loop_check",
        "ts": "2026-07-03T10:00:00Z",
        "source": "hook",
        "session_id": "session-a",
        "holder_session_id": None,
        "node_id": "x-1",
        "pr_number": 42,
        "short_id": None,
        "head_sha": None,
        "data": canonical["data"],
        "raw": canonical,
    }
    assert normalize_event(legacy)["data"] == legacy["payload"]
    assert normalize_event(legacy)["node_id"] == "x-1"
    assert normalize_event(legacy)["pr_number"] == 42
    claim_event = {
        "type": "claim_acquired",
        "data": {
            "key": "node:x-1",
            "holder": "target-session:session-a",
        },
    }
    assert normalize_event(claim_event)["node_id"] == "x-1"
    assert normalize_event(claim_event)["session_id"] is None
    assert normalize_event(claim_event)["holder_session_id"] == "session-a"

    path = tmp_path / "events.jsonl"
    path.write_text(
        __import__("json").dumps(canonical)
        + "\n"
        + __import__("json").dumps({**canonical, "data": {"session_id": "other"}})
        + "\n",
        encoding="utf-8",
    )
    assert read_events(path, session_id="session-a") == [canonical]


def test_trace_joins_context_to_objective_delivery_and_outcomes() -> None:
    delivery = {
        "session_id": "build-sess",
        "sessions": ["harness-session", "build-sess"],
        "graph_node_id": "x-1",
        "plan_path": "/plans/context.md",
        "commit_sha": "abc123",
        "evidence_receipt": {"scope": "full", "sha": "abc123"},
        "pr_number": 42,
        "pr_url": "https://github.com/acme/fno/pull/42",
        "completed": "2026-07-03T11:00:00",
        "duration_minutes": 12.5,
        "cost_usd": 3.25,
    }
    node = {"id": "x-1", "title": "Observe context", "reverted_at": None}
    events = [
        {
            "type": "claim_acquired",
            "ts": "2026-07-03T09:59:00Z",
            "source": "target",
            "data": {
                "key": "node:x-1",
                "holder": "target-session:build-sess",
                "pid": 123,
                "host": "worker",
                "acquired_at": 1783072740000,
            },
        },
        {
            "type": "context_snapshot",
            "ts": "2026-07-03T10:00:00Z",
            "source": "hook",
            "data": {
                "session_id": "harness-session",
                "harness": "codex",
                "entry_state": "startup",
                "context_bytes": 32000,
                "estimated_tokens": 8000,
                "context_hash": "ctx-hash",
                "source_hashes": ["one", "two"],
                "source_manifest": [],
                "measurement_complete": True,
            },
        },
        {
            "type": "loop_check",
            "ts": "2026-07-03T10:45:00Z",
            "source": "hook",
            "data": {
                "session_id": "build-sess",
                "fingerprint": "abc123 | OPEN | SUCCESS | 2026-07-03",
                "ci": "SUCCESS",
                "pr_state": "OPEN",
                "reviewed": True,
            },
        },
        {
            "type": "review_attestation",
            "ts": "2026-07-03T10:46:00Z",
            "source": "target",
            "data": {
                "reviewer": "sigma",
                "head_sha": "abc123",
                "verdict": "pass",
            },
        },
        {
            "type": "session_satisfied",
            "ts": "2026-07-03T10:50:00Z",
            "source": "target",
            "data": {
                "session_id": "build-sess",
                "source": "pr_merge",
                "reason": "merged",
                "gate_state_hash": "g",
            },
        },
        {
            "type": "recovery_nudge",
            "ts": "2026-07-03T10:20:00Z",
            "source": "hook",
            "data": {"session_id": "build-sess", "reason": "postcompact"},
        },
    ]

    trace = build_context_outcome_trace(delivery, node, events)

    assert trace["objective"] == "Observe context"
    assert trace["plan_node"] == "x-1"
    assert trace["worker_session"] == "build-sess"
    assert trace["claim"] == {
        "observed": True,
        "events": [
            {
                "type": "claim_acquired",
                "at": "2026-07-03T09:59:00Z",
                "key": "node:x-1",
                "holder": "target-session:build-sess",
            }
        ],
    }
    assert trace["commit"] == "abc123"
    assert trace["evidence_receipt"] == {"scope": "full", "sha": "abc123"}
    assert trace["pr"] == {
        "number": 42,
        "url": "https://github.com/acme/fno/pull/42",
    }
    assert trace["context"] == {
        "harness": "codex",
        "entry_state": "startup",
        "bytes": 32000,
        "estimated_tokens": 8000,
        "context_hash": "ctx-hash",
        "source_hashes": ["one", "two"],
        "measurement_complete": True,
        "measurement_errors": [],
    }
    assert trace["outcomes"]["ci"]["state"] == "SUCCESS"
    assert trace["outcomes"]["review"]["reviewed"] is True
    assert trace["outcomes"]["review"]["attestations"] == 1
    assert trace["outcomes"]["merge"] == {
        "observed": True,
        "state": "merged",
        "merged": True,
        "at": "2026-07-03T10:50:00Z",
    }
    assert trace["outcomes"]["revert"] == {
        "observed": False,
        "reverted": None,
        "at": None,
    }
    assert trace["outcomes"]["recovery"][0]["type"] == "recovery_nudge"
    assert trace["outcomes"]["latency_minutes"] == 12.5
    assert trace["outcomes"]["spend_usd"] == 3.25
    assert trace["falsifiable"] is True
    assert trace["provenance"] == ["ledger", "graph", "events"]


def test_missing_context_and_downstream_observations_never_fabricate_green() -> None:
    trace = build_context_outcome_trace(
        {
            "session_id": "s",
            "graph_node_id": "x-1",
            "termination_reason": "DonePRGreen",
        },
        {"id": "x-1", "title": "Unobserved"},
        [],
    )

    assert trace["context"] is None
    assert trace["falsifiable"] is False
    assert "context_snapshot" in trace["missing"]
    assert "downstream_outcome" in trace["missing"]


def test_explicit_false_revert_is_observed() -> None:
    trace = build_context_outcome_trace(
        {"session_id": "s", "graph_node_id": "x-1"},
        {
            "id": "x-1",
            "title": "Unknown outcome",
            "merge_status": None,
            "merged_at": None,
            "reverted": False,
            "reverted_at": None,
        },
        [],
    )

    assert trace["outcomes"]["merge"]["observed"] is False
    assert trace["outcomes"]["merge"]["merged"] is None
    assert trace["outcomes"]["revert"] == {
        "observed": True,
        "reverted": False,
        "at": None,
    }
    assert trace["falsifiable"] is True


def test_review_attestation_is_head_pinned_without_fixture_only_session_id() -> None:
    delivery = {
        "session_id": "run",
        "graph_node_id": "x-1",
        "commit_sha": "new-head",
    }
    events = [
        {
            "type": "review_attestation",
            "source": "target",
            "data": {
                "reviewer": "sigma",
                "head_sha": "old-head",
                "verdict": "pass",
            },
        },
        {
            "type": "review_attestation",
            "source": "target",
            "data": {
                "reviewer": "sigma",
                "head_sha": "new-head",
                "verdict": "pass",
            },
        },
    ]

    trace = build_context_outcome_trace(delivery, {"id": "x-1"}, events)

    assert trace["outcomes"]["review"]["attestations"] == 1
    assert trace["outcomes"]["review"]["reviewed"] is True


def test_recovery_short_id_joins_a_delivery_session_alias() -> None:
    trace = build_context_outcome_trace(
        {
            "session_id": "run",
            "sessions": ["abcdef12-3456-7890-abcd-ef1234567890", "run"],
            "graph_node_id": "x-1",
        },
        {"id": "x-1"},
        [
            {
                "type": "recovery_nudge",
                "source": "observer",
                "data": {"short_id": "abcdef12", "nudge_count": 1},
            }
        ],
    )

    assert trace["outcomes"]["recovery"][0]["type"] == "recovery_nudge"


def test_comparison_requires_a_predeclared_complete_contract() -> None:
    trace = {
        "worker_session": "candidate",
        "completed_at": "2026-07-03T11:00:00",
        "context": {"bytes": 50, "context_hash": "candidate-hash"},
        "outcomes": {"latency_minutes": 8, "spend_usd": 2},
        "falsifiable": True,
    }

    assert evaluate_context_comparison([trace], None) == {
        "label": "rejected",
        "reason": "comparison_contract_missing",
    }
    rejected = evaluate_context_comparison(
        [trace],
        {
            "declared_at": "2026-07-02T00:00:00Z",
            "recorded_at": "2026-07-02T00:00:00Z",
            "cohorts": {"baseline": [], "candidate": ["candidate"]},
            "window": {
                "start": "2026-07-01T00:00:00Z",
                "end": "2026-07-05T00:00:00Z",
            },
            "exclusions": [],
            "budgets": {"max_candidate_spend_usd": 5},
        },
    )
    assert rejected["label"] == "rejected"
    assert rejected["reason"] == "cohort_members_missing"


def test_latest_comparison_declaration_wins_across_historical_experiments() -> None:
    old = {"claim": "graph_widening", "cohorts": {"candidate": ["old"]}}
    latest = {"claim": "context_pruning", "cohorts": {"candidate": ["new"]}}
    events = [
        {
            "ts": "2026-07-03T00:00:00Z",
            "type": "context_comparison_declared",
            "source": "test",
            "data": {"comparison_contract": latest},
        },
        {
            "ts": "2026-07-01T00:00:00Z",
            "type": "context_comparison_declared",
            "source": "test",
            "data": {"comparison_contract": old},
        },
    ]

    assert _comparison_contract_from_events(events) == {
        **latest,
        "recorded_at": "2026-07-03T00:00:00Z",
    }


def test_predeclared_falsifiable_comparison_can_label_improvement() -> None:
    traces = [
        {
            "worker_session": "baseline",
            "completed_at": "2026-07-02T11:00:00",
            "context": {"bytes": 100, "context_hash": "baseline-hash"},
            "outcomes": {"latency_minutes": 10, "spend_usd": 2},
            "falsifiable": True,
        },
        {
            "worker_session": "candidate",
            "completed_at": "2026-07-03T11:00:00",
            "context": {"bytes": 60, "context_hash": "candidate-hash"},
            "outcomes": {"latency_minutes": 8, "spend_usd": 2.5},
            "falsifiable": True,
        },
    ]
    contract = {
        "declared_at": "2026-06-30T00:00:00Z",
        "recorded_at": "2026-06-30T00:00:01Z",
        "claim": "context_pruning",
        "cohorts": {
            "baseline": ["baseline"],
            "candidate": ["candidate"],
        },
        "window": {
            "start": "2026-07-01T00:00:00Z",
            "end": "2026-07-05T00:00:00Z",
        },
        "exclusions": [],
        "budgets": {"max_candidate_spend_usd": 3},
    }

    result = evaluate_context_comparison(traces, contract)

    assert result["label"] == "improvement"
    assert result["claim"] == "context_pruning"
    assert result["context_bytes"]["baseline_median"] == 100
    assert result["context_bytes"]["candidate_median"] == 60
    assert result["context_bytes"]["reduction_pct"] == 40


@pytest.mark.parametrize(
    "invalid_budget",
    [float("inf"), float("nan"), -1, 10**1000],
)
def test_comparison_rejects_nonfinite_or_negative_budgets(
    invalid_budget: float,
) -> None:
    contract = {
        "declared_at": "2026-06-30T00:00:00Z",
        "recorded_at": "2026-06-30T00:00:01Z",
        "cohorts": {"baseline": ["baseline"], "candidate": ["candidate"]},
        "window": {
            "start": "2026-07-01T00:00:00Z",
            "end": "2026-07-05T00:00:00Z",
        },
        "exclusions": [],
        "budgets": {"max_candidate_spend_usd": invalid_budget},
    }

    assert evaluate_context_comparison([], contract) == {
        "label": "rejected",
        "reason": "comparison_budgets_invalid",
    }


@pytest.mark.parametrize(
    ("claim", "path", "invalid_value", "reason"),
    [
        (
            "context_pruning",
            ("context", "bytes"),
            float("inf"),
            "context_measurement_missing",
        ),
        (
            "graph_widening",
            ("outcomes", "latency_minutes"),
            -1,
            "latency_measurement_missing",
        ),
    ],
)
def test_comparison_rejects_invalid_improvement_observations(
    claim: str,
    path: tuple[str, str],
    invalid_value: float,
    reason: str,
) -> None:
    traces = [
        {
            "worker_session": "baseline",
            "completed_at": "2026-07-02T11:00:00",
            "context": {"bytes": 100},
            "outcomes": {"latency_minutes": 10, "spend_usd": 1},
            "falsifiable": True,
        },
        {
            "worker_session": "candidate",
            "completed_at": "2026-07-03T11:00:00",
            "context": {"bytes": 50},
            "outcomes": {"latency_minutes": 5, "spend_usd": 1},
            "falsifiable": True,
        },
    ]
    traces[1][path[0]][path[1]] = invalid_value
    contract = {
        "declared_at": "2026-06-30T00:00:00Z",
        "recorded_at": "2026-06-30T00:00:01Z",
        "claim": claim,
        "cohorts": {"baseline": ["baseline"], "candidate": ["candidate"]},
        "window": {
            "start": "2026-07-01T00:00:00Z",
            "end": "2026-07-05T00:00:00Z",
        },
        "exclusions": [],
        "budgets": {"max_candidate_spend_usd": 2},
    }

    assert evaluate_context_comparison(traces, contract)["reason"] == reason


@pytest.mark.parametrize("invalid_spend", [float("inf"), -1, 10**1000])
def test_comparison_rejects_invalid_spend_observation(
    invalid_spend: float,
) -> None:
    traces = [
        {
            "worker_session": "baseline",
            "completed_at": "2026-07-02T11:00:00",
            "context": {"bytes": 100},
            "outcomes": {"spend_usd": 1},
            "falsifiable": True,
        },
        {
            "worker_session": "candidate",
            "completed_at": "2026-07-03T11:00:00",
            "context": {"bytes": 50},
            "outcomes": {"spend_usd": invalid_spend},
            "falsifiable": True,
        },
    ]
    contract = {
        "declared_at": "2026-06-30T00:00:00Z",
        "recorded_at": "2026-06-30T00:00:01Z",
        "cohorts": {"baseline": ["baseline"], "candidate": ["candidate"]},
        "window": {
            "start": "2026-07-01T00:00:00Z",
            "end": "2026-07-05T00:00:00Z",
        },
        "exclusions": [],
        "budgets": {"max_candidate_spend_usd": 2},
    }

    assert evaluate_context_comparison(traces, contract)["reason"] == (
        "comparison_budget_measurement_missing"
    )


@pytest.mark.parametrize("invalid_findings", [-1, True])
def test_comparison_rejects_invalid_review_finding_observation(
    invalid_findings: object,
) -> None:
    traces = [
        {
            "worker_session": session,
            "completed_at": completed,
            "context": {"bytes": context_bytes},
            "outcomes": {
                "review": {"observed": True, "findings": findings},
            },
            "falsifiable": True,
        }
        for session, completed, context_bytes, findings in (
            ("baseline", "2026-07-02T11:00:00", 100, 0),
            ("candidate", "2026-07-03T11:00:00", 50, invalid_findings),
        )
    ]
    contract = {
        "declared_at": "2026-06-30T00:00:00Z",
        "recorded_at": "2026-06-30T00:00:01Z",
        "cohorts": {"baseline": ["baseline"], "candidate": ["candidate"]},
        "window": {
            "start": "2026-07-01T00:00:00Z",
            "end": "2026-07-05T00:00:00Z",
        },
        "exclusions": [],
        "budgets": {"max_candidate_review_findings": 0},
    }

    assert evaluate_context_comparison(traces, contract)["reason"] == (
        "comparison_budget_measurement_missing"
    )


def test_huge_or_nonfinite_ledger_numbers_are_unmeasurable() -> None:
    assert _num_opt(10**1000) is None
    assert _num_opt(float("inf")) is None
    assert _num_opt(-1) is None
    assert _num_opt(True) is None


@pytest.mark.parametrize(
    ("field", "budget"),
    [
        ("cost_usd", "max_candidate_spend_usd"),
        ("duration_minutes", "max_candidate_latency_minutes"),
    ],
)
def test_boolean_ledger_metric_fails_closed_through_trace_comparison(
    field: str,
    budget: str,
) -> None:
    deliveries = [
        {
            "session_id": "baseline",
            "completed": "2026-07-02T11:00:00",
            "duration_minutes": 2,
            "cost_usd": 1,
        },
        {
            "session_id": "candidate",
            "completed": "2026-07-03T11:00:00",
            "duration_minutes": 1,
            "cost_usd": 1,
            field: True,
        },
    ]
    traces = [
        build_context_outcome_trace(
            delivery,
            {"id": f"x-{index}", "title": session, "reverted": False},
            [_snapshot_event(session, context_bytes, "2026-07-01T10:00:00Z")],
        )
        for index, (delivery, session, context_bytes) in enumerate(
            zip(deliveries, ("baseline", "candidate"), (100, 50)),
            start=1,
        )
    ]
    assert traces[1]["outcomes"][
        "spend_usd" if field == "cost_usd" else "latency_minutes"
    ] is None
    contract = {
        "declared_at": "2026-06-30T00:00:00Z",
        "recorded_at": "2026-06-30T00:00:01Z",
        "cohorts": {"baseline": ["baseline"], "candidate": ["candidate"]},
        "window": {
            "start": "2026-07-01T00:00:00Z",
            "end": "2026-07-05T00:00:00Z",
        },
        "exclusions": [],
        "budgets": {budget: 2},
    }

    assert evaluate_context_comparison(traces, contract) == {
        "label": "rejected",
        "reason": "comparison_budget_measurement_missing",
    }


def test_comparison_rejects_finite_spend_aggregate_overflow() -> None:
    traces = [
        {
            "worker_session": session,
            "completed_at": completed,
            "context": {"bytes": context_bytes},
            "outcomes": {"spend_usd": spend},
            "falsifiable": True,
        }
        for session, completed, context_bytes, spend in (
            ("baseline", "2026-07-02T11:00:00", 100, 0),
            ("candidate-a", "2026-07-03T10:00:00", 50, sys.float_info.max),
            ("candidate-b", "2026-07-03T11:00:00", 50, sys.float_info.max),
        )
    ]
    contract = {
        "declared_at": "2026-06-30T00:00:00Z",
        "recorded_at": "2026-06-30T00:00:01Z",
        "cohorts": {
            "baseline": ["baseline"],
            "candidate": ["candidate-a", "candidate-b"],
        },
        "window": {
            "start": "2026-07-01T00:00:00Z",
            "end": "2026-07-05T00:00:00Z",
        },
        "exclusions": [],
        "budgets": {"max_candidate_spend_usd": sys.float_info.max},
    }

    assert evaluate_context_comparison(traces, contract)["reason"] == (
        "comparison_budget_measurement_missing"
    )


def test_comparison_rejects_nonfinite_median_or_reduction() -> None:
    contract = {
        "declared_at": "2026-06-30T00:00:00Z",
        "recorded_at": "2026-06-30T00:00:01Z",
        "cohorts": {
            "baseline": ["baseline-a", "baseline-b"],
            "candidate": ["candidate"],
        },
        "window": {
            "start": "2026-07-01T00:00:00Z",
            "end": "2026-07-05T00:00:00Z",
        },
        "exclusions": [],
        "budgets": {"max_candidate_spend_usd": 1},
    }
    traces = [
        {
            "worker_session": session,
            "completed_at": completed,
            "context": {"bytes": context_bytes},
            "outcomes": {"spend_usd": 0},
            "falsifiable": True,
        }
        for session, completed, context_bytes in (
            ("baseline-a", "2026-07-02T10:00:00", sys.float_info.max),
            ("baseline-b", "2026-07-02T11:00:00", sys.float_info.max),
            ("candidate", "2026-07-03T11:00:00", 1),
        )
    ]
    assert evaluate_context_comparison(traces, contract)["reason"] == (
        "context_measurement_missing"
    )

    contract["cohorts"]["baseline"] = ["baseline-a"]
    traces[0]["context"]["bytes"] = 5e-324
    traces[2]["context"]["bytes"] = sys.float_info.max
    assert evaluate_context_comparison(traces, contract)["reason"] == (
        "context_measurement_missing"
    )


def test_comparison_rejects_missing_declared_member_instead_of_using_subset() -> None:
    traces = [
        {
            "worker_session": "baseline-present",
            "completed_at": "2026-07-02T11:00:00",
            "context": {"bytes": 100, "measurement_complete": True},
            "outcomes": {"spend_usd": 1},
            "falsifiable": True,
        },
        {
            "worker_session": "candidate",
            "completed_at": "2026-07-03T11:00:00",
            "context": {"bytes": 50, "measurement_complete": True},
            "outcomes": {"spend_usd": 1},
            "falsifiable": True,
        },
    ]
    contract = {
        "declared_at": "2026-06-30T00:00:00Z",
        "recorded_at": "2026-06-30T00:00:01Z",
        "claim": "context_pruning",
        "cohorts": {
            "baseline": ["baseline-present", "baseline-missing"],
            "candidate": ["candidate"],
        },
        "window": {
            "start": "2026-07-01T00:00:00Z",
            "end": "2026-07-05T00:00:00Z",
        },
        "exclusions": [],
        "budgets": {"max_candidate_spend_usd": 2},
    }

    assert evaluate_context_comparison(traces, contract)["reason"] == "cohort_members_missing"


def test_comparison_rejects_incomplete_context_measurement() -> None:
    traces = [
        {
            "worker_session": "baseline",
            "completed_at": "2026-07-02T11:00:00",
            "context": {"bytes": 100, "measurement_complete": True},
            "outcomes": {"spend_usd": 1},
            "falsifiable": True,
        },
        {
            "worker_session": "candidate",
            "completed_at": "2026-07-03T11:00:00",
            "context": {
                "bytes": 0,
                "measurement_complete": False,
                "measurement_errors": ["missing-source"],
            },
            "outcomes": {"spend_usd": 1},
            "falsifiable": True,
        },
    ]
    contract = {
        "declared_at": "2026-06-30T00:00:00Z",
        "recorded_at": "2026-06-30T00:00:01Z",
        "claim": "context_pruning",
        "cohorts": {"baseline": ["baseline"], "candidate": ["candidate"]},
        "window": {
            "start": "2026-07-01T00:00:00Z",
            "end": "2026-07-05T00:00:00Z",
        },
        "exclusions": [],
        "budgets": {"max_candidate_spend_usd": 2},
    }

    assert (
        evaluate_context_comparison(traces, contract)["reason"]
        == "context_measurement_incomplete"
    )


def test_comparison_rejects_incomplete_event_journal_coverage() -> None:
    contract = {
        "declared_at": "2026-06-30T00:00:00Z",
        "recorded_at": "2026-06-30T00:00:01Z",
        "cohorts": {"baseline": ["baseline"], "candidate": ["candidate"]},
        "window": {
            "start": "2026-07-01T00:00:00Z",
            "end": "2026-07-05T00:00:00Z",
        },
        "exclusions": [],
        "budgets": {"max_candidate_spend_usd": 2},
    }

    assert evaluate_context_comparison(
        [], contract, observation_complete=False
    ) == {
        "label": "rejected",
        "reason": "event_observation_incomplete",
    }


def test_risk_budget_can_prevent_an_improvement_label() -> None:
    traces = [
        {
            "worker_session": "baseline",
            "completed_at": "2026-07-02T11:00:00",
            "context": {"bytes": 100},
            "outcomes": {
                "latency_minutes": 10,
                "spend_usd": 2,
                "review": {"observed": True, "findings": 0},
                "revert": {"observed": True, "reverted": False},
            },
            "falsifiable": True,
        },
        {
            "worker_session": "candidate",
            "completed_at": "2026-07-03T11:00:00",
            "context": {"bytes": 60},
            "outcomes": {
                "latency_minutes": 8,
                "spend_usd": 2,
                "review": {"observed": True, "findings": 2},
                "revert": {"observed": True, "reverted": False},
            },
            "falsifiable": True,
        },
    ]
    result = evaluate_context_comparison(
        traces,
        {
            "declared_at": "2026-06-30T00:00:00Z",
            "recorded_at": "2026-06-30T00:00:01Z",
            "claim": "context_pruning",
            "cohorts": {"baseline": ["baseline"], "candidate": ["candidate"]},
            "window": {
                "start": "2026-07-01T00:00:00Z",
                "end": "2026-07-05T00:00:00Z",
            },
            "exclusions": [],
            "budgets": {"max_candidate_review_findings": 0},
        },
    )

    assert result["label"] == "no_improvement"
    assert result["budget_satisfied"] is False
    assert result["budget_observations"] == {
        "max_candidate_review_findings": 2.0
    }


def test_plan_fidelity_includes_the_derived_trace() -> None:
    rows = [
        {
            "completed": "2026-07-03T10:00:00",
            "termination_reason": "NoWork",
            "phases_completed": ["think", "plan"],
            "plan_path": "/wt-a/context/context.md",
            "project": "fno",
            "session_id": "plan-sess",
        },
        {
            "completed": "2026-07-03T11:00:00",
            "termination_reason": "DonePRGreen",
            "phases_completed": ["do", "ship"],
            "plan_path": "/wt-b/context/context.md",
            "project": "fno",
            "graph_node_id": "x-1",
            "session_id": "build-sess",
            "pr_number": 42,
            "duration_minutes": 5,
            "cost_usd": 1,
        },
    ]
    events = [
        {
            "ts": "2026-07-03T10:30:00",
            "type": "context_snapshot",
            "data": {
                "session_id": "build-sess",
                "context_bytes": 100,
                "estimated_tokens": 25,
                "context_hash": "h",
                "source_hashes": ["s"],
                "source_manifest": [],
                "measurement_complete": True,
            },
        }
    ]
    result = build_plan_fidelity(
        rows,
        [{"id": "x-1", "title": "Observe context"}],
        since_days=28,
        now=NOW,
        read_plan_doc=lambda _path: "## Acceptance Criteria\n#### AC1-HP: yes\n",
        read_summary=lambda _row: "AC1-HP",
        read_diff=lambda _row: [],
        trace_events=events,
    )

    joined = next(item for item in result["results"] if item["status"] == "joined")
    assert joined["context_outcome_trace"]["context"]["context_hash"] == "h"
    assert joined["context_outcome_trace"]["plan_node"] == "x-1"
    assert result["context_comparison"]["label"] == "rejected"


def test_event_reader_deduplicates_journals_and_reports_corruption(tmp_path: Path) -> None:
    event = _snapshot_event("s", 1, "2026-07-03T10:00:00Z")
    first = tmp_path / "project.jsonl"
    second = tmp_path / "global.jsonl"
    encoded = __import__("json").dumps(event)
    first.write_text(encoded + "\n", encoding="utf-8")
    second.write_text(encoded + "\n{partial\n", encoding="utf-8")

    result = read_jsonl_events_with_coverage(
        [first, second], {"context_snapshot"}
    )

    assert result["events"] == [event]
    assert result["coverage"]["complete"] is False
    assert result["coverage"]["malformed_lines"] == 1


@pytest.mark.parametrize("non_object", ["[]", "null", '"scalar"'])
def test_event_reader_counts_non_object_json_as_malformed(
    tmp_path: Path,
    non_object: str,
) -> None:
    path = tmp_path / "events.jsonl"
    path.write_text(non_object + "\n", encoding="utf-8")

    result = read_jsonl_events_with_coverage([path], {"context_snapshot"})

    assert result["events"] == []
    assert result["coverage"]["complete"] is False
    assert result["coverage"]["malformed_lines"] == 1


@pytest.mark.parametrize("invalid_data", [1, "scalar", []])
def test_event_reader_counts_non_object_event_data_as_malformed(
    tmp_path: Path,
    invalid_data: object,
) -> None:
    path = tmp_path / "events.jsonl"
    event = {
        "ts": "2026-07-26T01:00:00Z",
        "type": "context_snapshot",
        "source": "hook",
        "data": invalid_data,
    }
    path.write_text(json.dumps(event) + "\n", encoding="utf-8")

    result = read_jsonl_events_with_coverage([path], {"context_snapshot"})

    assert result["events"] == []
    assert result["coverage"]["complete"] is False
    assert result["coverage"]["malformed_lines"] == 1


def test_event_reader_orders_latest_observation_by_canonical_timestamp(
    tmp_path: Path,
) -> None:
    older = _snapshot_event("s", 100, "2026-07-03T10:00:00Z")
    newer = _snapshot_event("s", 60, "2026-07-03T11:00:00Z")
    first = tmp_path / "global.jsonl"
    second = tmp_path / "delivery.jsonl"
    first.write_text(json.dumps(newer) + "\n", encoding="utf-8")
    second.write_text(
        json.dumps(older) + "\n" + json.dumps(newer) + "\n",
        encoding="utf-8",
    )

    result = read_jsonl_events_with_coverage(
        [first, second], {"context_snapshot"}
    )
    trace = build_context_outcome_trace(
        {"session_id": "s", "commit_sha": "head"},
        None,
        result["events"],
    )

    assert result["events"] == [older, newer]
    assert trace["context"]["bytes"] == 60
    assert trace["context"]["context_hash"] == newer["data"]["context_hash"]


def test_reused_harness_session_cannot_leak_future_context_backward() -> None:
    events = [
        _snapshot_event("shared-thread", 10, "2026-07-03T10:00:00-07:00"),
        _snapshot_event("shared-thread", 20, "2026-07-03T11:00:00-07:00"),
    ]
    first = build_context_outcome_trace(
        {
            "session_id": "delivery-one",
            "sessions": ["delivery-one", "shared-thread"],
            "completed": "2026-07-03T10:30:00-07:00",
        },
        None,
        events,
    )
    second = build_context_outcome_trace(
        {
            "session_id": "delivery-two",
            "sessions": ["delivery-two", "shared-thread"],
            "completed": "2026-07-03T11:30:00-07:00",
        },
        None,
        events,
    )

    assert first["context"]["bytes"] == 10
    assert second["context"]["bytes"] == 20


def test_same_timestamp_complete_snapshot_supersedes_incomplete() -> None:
    complete = _snapshot_event("s", 10, "2026-07-03T10:00:00Z")
    incomplete = deepcopy(complete)
    incomplete["data"].update(
        context_bytes=0,
        estimated_tokens=0,
        context_hash=None,
        source_hashes=[],
        source_manifest=[
            {
                "source_id": "fixture",
                "status": "unreadable",
                "bytes": 0,
                "content_hash": None,
            }
        ],
        measurement_complete=False,
        measurement_errors=["fixture: missing"],
    )

    trace = build_context_outcome_trace(
        {"session_id": "s", "commit_sha": "head"},
        None,
        [complete, incomplete],
    )

    assert trace["context"]["bytes"] == 10
    assert trace["context"]["measurement_complete"] is True


def test_recorded_merge_failure_is_observed_and_falsifiable() -> None:
    trace = build_context_outcome_trace(
        {"session_id": "s", "commit_sha": "head"},
        {"id": "x-1", "merge_status": "failed"},
        [],
    )

    assert trace["outcomes"]["merge"] == {
        "observed": True,
        "state": "failed",
        "merged": False,
        "at": None,
    }
    assert trace["falsifiable"] is True


def test_queued_merge_is_observed_and_falsifiable() -> None:
    trace = build_context_outcome_trace(
        {"session_id": "s", "commit_sha": "head"},
        {"id": "x-1", "merge_status": "queued"},
        [],
    )

    assert trace["outcomes"]["merge"] == {
        "observed": True,
        "state": "queued",
        "merged": False,
        "at": None,
    }
    assert trace["falsifiable"] is True


@pytest.mark.parametrize("stale_status", ["queued", "failed"])
def test_terminal_merge_evidence_overrides_stale_graph_state(
    stale_status: str,
) -> None:
    loop = {
        "ts": "2026-07-03T11:00:00Z",
        "type": "loop_check",
        "source": "hook",
        "data": {
            "session_id": "s",
            "pr_state": "MERGED",
            "ci": "SUCCESS",
            "reviewed": True,
        },
    }
    trace = build_context_outcome_trace(
        {"session_id": "s", "commit_sha": "head"},
        {"id": "x-1", "merge_status": stale_status},
        [loop],
    )

    assert trace["outcomes"]["merge"]["state"] == "merged"
    assert trace["outcomes"]["merge"]["merged"] is True


def test_registered_codex_harness_identity_joins_runtime_snapshot(
    tmp_path: Path,
    monkeypatch,
) -> None:
    from fno.cost import _register

    worktree = tmp_path / "worktree"
    canonical = tmp_path / "canonical"
    worktree.mkdir()
    canonical.mkdir()

    def fake_git(*args: str) -> str:
        return {
            ("branch", "--show-current"): "feature/x-1",
            ("remote", "get-url", "origin"): "",
            ("rev-parse", "--show-toplevel"): str(worktree),
        }.get(args, "")

    monkeypatch.setattr(_register, "git_cmd", fake_git)
    monkeypatch.setattr(
        _register._paths,
        "resolve_canonical_worktree",
        lambda *_args, **_kwargs: canonical,
    )
    monkeypatch.setattr(_register, "_pr_number_from_ship_artifact", lambda *_: None)
    monkeypatch.setattr(_register, "_pr_number_from_gh", lambda *_: None)
    monkeypatch.setattr(_register, "sum_plan_points", lambda *_: 0)
    thread_id = "019f905a-9627-7f51-88c0-d3357f6ef31b"
    entry = _register.build_entry(
        {
            "fno_id": "20260723T202108Z-cx83069-27f912",
            "harness_session_id": thread_id,
            "codex_thread_id": thread_id,
            "graph_node_id": "x-1",
        },
        f"rollout-2026-07-23-{thread_id}",
    )
    trace = build_context_outcome_trace(
        entry,
        {"id": "x-1"},
        [_snapshot_event(thread_id, 42, "2026-07-03T10:00:00Z")],
    )

    assert entry["sessions"] == [
        f"rollout-2026-07-23-{thread_id}",
        "20260723T202108Z-cx83069-27f912",
        thread_id,
    ]
    assert entry["canonical_root_path"] == str(canonical.resolve())
    assert trace["context"]["bytes"] == 42


def test_scoreboard_cli_joins_canonical_project_context_journal(
    tmp_path: Path,
) -> None:
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    repo = tmp_path / "repo"
    (repo / ".fno").mkdir(parents=True)
    plan = tmp_path / "plans" / "context.md"
    plan.parent.mkdir()
    plan.write_text("## Acceptance Criteria\n#### AC1-HP: observe context\n", encoding="utf-8")
    now = datetime.now().replace(microsecond=0)
    planning_at = (now - timedelta(hours=2)).isoformat()
    delivery_at = (now - timedelta(hours=1)).isoformat()
    rows = [
        {
            "completed": planning_at,
            "phases_completed": ["think", "plan"],
            "plan_path": str(plan),
            "project": "fixture",
            "session_id": "plan-session",
        },
        {
            "completed": delivery_at,
            "termination_reason": "DonePRGreen",
            "phases_completed": ["do", "ship"],
            "plan_path": str(plan),
            "project": "fixture",
            "graph_node_id": "x-1",
            "session_id": "run-session",
            "sessions": ["harness-session", "run-session"],
            "root_path": str(repo),
            "commit_sha": "abc123",
            "duration_minutes": 5,
            "cost_usd": 1,
        },
    ]
    (state_dir / "ledger.json").write_text(
        json.dumps({"entries": rows}), encoding="utf-8"
    )
    (state_dir / "graph.json").write_text(
        json.dumps({"entries": [{"id": "x-1", "title": "Observe context"}]}),
        encoding="utf-8",
    )
    event_at = (now - timedelta(minutes=90)).astimezone().isoformat()
    event = _snapshot_event("harness-session", 100, event_at)
    (repo / ".fno" / "events.jsonl").write_text(
        json.dumps(event) + "\n", encoding="utf-8"
    )
    settings = tmp_path / "settings.yaml"
    settings.write_text(
        "schema_version: 1\n"
        "config:\n"
        f"  state_dir: {state_dir}/\n",
        encoding="utf-8",
    )
    executable = Path(sys.executable).parent / "fno-py"
    result = subprocess.run(
        [str(executable), "scoreboard", "--plan-fidelity", "--json", "--since", "2"],
        cwd=repo,
        env={
            **os.environ,
            "FNO_CONFIG": str(settings),
            "FNO_REPO_ROOT": str(repo),
            "FNO_TEST_MODE": "1",
        },
        text=True,
        capture_output=True,
        check=False,
        timeout=30,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    joined = next(item for item in payload["results"] if item["status"] == "joined")
    assert joined["context_outcome_trace"]["context"]["bytes"] == 100
    assert (
        joined["context_outcome_trace"]["context"]["context_hash"]
        == event["data"]["context_hash"]
    )


@pytest.mark.parametrize("archive_second", [False, True])
def test_scoreboard_cli_inventories_live_and_archived_delivery_roots(
    tmp_path: Path,
    archive_second: bool,
) -> None:
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    roots = [tmp_path / "repo-a", tmp_path / "repo-b"]
    repo_b_canonical = tmp_path / "repo-b-canonical"
    for root in roots[:1] if archive_second else roots:
        (root / ".fno").mkdir(parents=True)
    plans = [tmp_path / "plans" / "a.md", tmp_path / "plans" / "b.md"]
    plans[0].parent.mkdir()
    for plan in plans:
        plan.write_text(
            "## Acceptance Criteria\n#### AC1-HP: observe context\n",
            encoding="utf-8",
        )
    completed = datetime.now().replace(microsecond=0).isoformat()
    rows = []
    for index, (root, plan) in enumerate(zip(roots, plans), start=1):
        project = f"fixture-{index}"
        delivery = {
            "completed": completed,
            "termination_reason": "DonePRGreen",
            "phases_completed": ["do", "ship"],
            "plan_path": str(plan),
            "project": project,
            "graph_node_id": f"x-{index}",
            "session_id": f"run-{index}",
            "root_path": str(root),
            "commit_sha": f"head-{index}",
            "duration_minutes": 5,
            "cost_usd": 1,
        }
        if archive_second and index == 2:
            delivery["graph_node_id"] = None
            delivery["canonical_root_path"] = str(repo_b_canonical)
        rows.extend(
            [
                {
                    "completed": completed,
                    "phases_completed": ["think", "plan"],
                    "plan_path": str(plan),
                    "project": project,
                    "session_id": f"plan-{index}",
                },
                delivery,
            ]
        )
    (state_dir / "ledger.json").write_text(
        json.dumps({"entries": rows}), encoding="utf-8"
    )
    (state_dir / "graph.json").write_text(
        json.dumps(
            {
                "entries": [
                    {"id": "x-1", "title": "First root"},
                    {"id": "x-2", "title": "Second root"},
                ]
            }
        ),
        encoding="utf-8",
    )

    duplicate_snapshot = _snapshot_event("run-1", 110, "2026-07-03T11:00:00Z")
    duplicate_review = {
        "ts": "2026-07-03T11:01:00Z",
        "type": "review_attestation",
        "source": "target",
        "data": {
            "reviewer": "sigma",
            "head_sha": "head-1",
            "verdict": "pass",
        },
    }
    (state_dir / "events.jsonl").write_text(
        json.dumps(duplicate_snapshot) + "\n" + json.dumps(duplicate_review) + "\n",
        encoding="utf-8",
    )
    (roots[0] / ".fno" / "events.jsonl").write_text(
        json.dumps(duplicate_snapshot)
        + "\n"
        + json.dumps(_snapshot_event("run-1", 90, "2026-07-03T10:00:00Z"))
        + "\n"
        + json.dumps(duplicate_review)
        + "\n",
        encoding="utf-8",
    )
    second_event_path = roots[1] / ".fno" / "events.jsonl"
    archived_first_path = None
    if archive_second:
        second_event_path = (
            repo_b_canonical
            / ".fno"
            / "salvage"
            / "20260725-feature-plan-only"
            / "events.jsonl"
        )
        second_event_path.parent.mkdir(parents=True)
        archived_first_path = (
            roots[0] / ".fno" / "salvage" / "20260724-x-1" / "events.jsonl"
        )
        archived_first_path.parent.mkdir(parents=True)
        archived_first_path.write_text(
            json.dumps(_snapshot_event("run-1", 80, "2026-07-03T09:00:00Z")) + "\n",
            encoding="utf-8",
        )
    second_event_path.write_text(
        json.dumps(_snapshot_event("run-2", 220, "2026-07-03T12:00:00Z")) + "\n",
        encoding="utf-8",
    )
    settings = tmp_path / "settings.yaml"
    settings.write_text(
        "schema_version: 1\n"
        "config:\n"
        f"  state_dir: {state_dir}/\n",
        encoding="utf-8",
    )
    executable = Path(sys.executable).parent / "fno-py"

    result = subprocess.run(
        [str(executable), "scoreboard", "--plan-fidelity", "--json", "--since", "2"],
        cwd=roots[0],
        env={
            **os.environ,
            "FNO_CONFIG": str(settings),
            "FNO_REPO_ROOT": str(roots[0]),
            "FNO_TEST_MODE": "1",
        },
        text=True,
        capture_output=True,
        check=False,
        timeout=30,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    joined = {
        item["context_outcome_trace"]["worker_session"]: item["context_outcome_trace"]
        for item in payload["results"]
        if item["status"] == "joined"
    }
    assert joined["run-1"]["context"]["bytes"] == 110
    assert joined["run-1"]["outcomes"]["review"]["attestations"] == 1
    assert joined["run-2"]["context"]["bytes"] == 220
    observed_paths = {
        item["path"]
        for item in payload["event_coverage"]["paths"]
        if item["status"] == "ok"
    }
    expected_paths = {
        str((state_dir / "events.jsonl").resolve()),
        str((roots[0] / ".fno" / "events.jsonl").resolve()),
        str(second_event_path.resolve()),
    }
    if archived_first_path is not None:
        expected_paths.add(str(archived_first_path.resolve()))
    assert observed_paths == expected_paths


def test_evals_documentation_is_generated_from_the_trace_contract() -> None:
    docs = (ROOT / "docs" / "evals.md").read_text(encoding="utf-8")
    start = "<!-- context-outcome-fields:start -->"
    end = "<!-- context-outcome-fields:end -->"
    assert start in docs and end in docs
    actual = docs.split(start, 1)[1].split(end, 1)[0].strip()
    assert actual == render_context_trace_field_docs().strip()
