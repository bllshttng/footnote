"""Hermetic classifier contract for the isolated Codex continuation receipt."""

from __future__ import annotations

import importlib.util
from copy import deepcopy
from datetime import datetime, timezone
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).parents[2]
DIAGNOSTIC = REPO_ROOT / "scripts" / "diagnostics" / "codex-reign-continuation-smoke.py"


def load_diagnostic():
    assert DIAGNOSTIC.is_file(), f"diagnostic is missing: {DIAGNOSTIC}"
    spec = importlib.util.spec_from_file_location("codex_reign_continuation_smoke", DIAGNOSTIC)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def verified_receipt() -> dict:
    return {
        "schema_version": 2,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "versions": {"codex": "codex-cli 0.155.1", "fno": "0.3.2", "fno_agents": "0.3.2"},
        "session": {
            "id": "01a00000-0000-7000-8000-000000000001",
            "harness": "codex",
            "turn_ids": ["turn-0001", "turn-0002", "turn-0003"],
            "identity_constant": True,
        },
        "correlation_id": "stop-correlation-0001",
        "continuation_owner": "stop",
        "action_hash": "sha256:nonce-action-0001",
        "user_message_count": 1,
        "command_requests": [
            {"command": "/compact", "status": "verified", "request_id": "compact-0001"},
            {"command": "/goal", "status": "verified", "request_id": "goal-0001"},
        ],
        "window": {
            "requested": 1_000_000,
            "default": 272_000,
            "max": 872_000,
            "percent": 0.95,
            "effective": 828_400,
            "source": "explicit-per-thread",
            "no_request_control_effective": 258_400,
            "cost_policy": "272K",
        },
        "goal": {
            "before": {"status": "absent", "objective": None, "usage": 0},
            "after": {
                "status": "active",
                "objective": "$fno:reign disposable",
                "usage": 1,
                "thread_id": "01a00000-0000-7000-8000-000000000001",
            },
            "paused": {
                "status": "paused",
                "objective": "$fno:reign disposable",
                "usage": 1,
            },
            "resumed": {
                "status": "active",
                "objective": "$fno:reign disposable",
                "usage": 1,
            },
        },
        "stop": {
            "independent": {
                "decision": "block",
                "class": "actionable-block",
                "correlation_id": "stop-correlation-0001",
                "goal_before": "absent",
                "useful_action_after_block": True,
                "action_order": ["stop-block", "nonce-write"],
            },
            "delegated": {
                "decision": "allow",
                "class": "delegated-to-goal",
                "continuation_owner": "goal",
                "block_count": 0,
                "useful_action": True,
            },
        },
        "proof": {
            "mail_count": 0,
            "queue_count": 0,
            "manual_submit_count": 0,
            "native_goal_initially_absent": True,
            "independent_stop": True,
            "goal_delegation": True,
            "useful_nonce_action": True,
            "user_message_count": 1,
            "quiet_park": {
                "park_count": 1,
                "stop_samples_during_hold": 0,
                "park_interval_seconds": 0.25,
                "wake_result": "resumed",
            },
            "repeats": [
                {"boundary": "compaction", "status": "verified", "same_session": True, "useful_action": True},
                {"boundary": "resume", "status": "verified", "same_session": True, "useful_action": True},
                {"boundary": "private-daemon-replacement", "status": "verified", "same_session": True, "useful_action": True},
            ],
        },
        "status": "verified",
    }


def test_ac10_hp_and_ac11_hp_require_independent_stop_and_single_user_message():
    """AC10-HP/AC11-HP: both continuation owners are proven without input injection."""
    result = load_diagnostic().classify_receipt(verified_receipt())

    assert result["ok"] is True
    assert result["class"] == "verified-continuation"
    assert result["failed_reader"] is None


def test_ac10_edge_requires_compaction_resume_and_daemon_replacement_repeats():
    """AC10-EDGE: every private boundary repeats useful work under one identity."""
    receipt = verified_receipt()
    receipt["proof"]["repeats"][1]["same_session"] = False

    result = load_diagnostic().classify_receipt(receipt)

    assert result == {
        "ok": False,
        "class": "resume-marker-stale",
        "failed_reader": "resume.lifecycle_marker",
    }


def test_ac10_park_and_ac11_park_require_one_park_zero_samples_and_one_wake():
    """AC10-PARK/AC11-PARK: quiet hold does not spin and wake resumes the same goal."""
    receipt = verified_receipt()
    receipt["proof"]["quiet_park"]["stop_samples_during_hold"] = 1

    result = load_diagnostic().classify_receipt(receipt)

    assert result["ok"] is False
    assert result["class"] == "explicit-park"
    assert result["failed_reader"] == "quiet_park.stop_samples_during_hold"


def test_ac3_resume_preserves_window_facts_after_private_restart():
    """AC3-RESUME: the explicit window survives with the 828400 effective value."""
    result = load_diagnostic().classify_receipt(verified_receipt())

    assert result["ok"] is True
    assert result["window"] == {"requested": 1_000_000, "effective": 828_400}


@pytest.mark.parametrize(
    ("failure_class", "reader"),
    [
        ("plugin-missing", "machine.plugin"),
        ("machine-installed-session-refresh-unverified", "session.discovery"),
        ("hooks-disabled", "session.hooks"),
        ("session-hook-unobserved", "lifecycle.context_snapshot"),
        ("identity-miss", "identity.registry"),
        ("malformed-output", "stop.output"),
        ("hook-timeout", "stop.timeout"),
        ("parser-rejected", "stop.parser"),
        ("wake-disabled", "wake.trigger"),
        ("wake-budget-spent", "wake.budget"),
        ("wake-refused", "wake.provider"),
        ("compaction-marker-stale", "compaction.lifecycle_marker"),
    ],
)
def test_ac8_err_has_exactly_one_primary_failure_class(failure_class: str, reader: str):
    """AC8-ERR: every negative fixture names one failed reader, never success."""
    receipt = verified_receipt()
    receipt["status"] = "refused"
    receipt["failure"] = {"class": failure_class, "reader": reader}

    result = load_diagnostic().classify_receipt(receipt)

    assert result == {"ok": False, "class": failure_class, "failed_reader": reader}


def test_missing_positive_evidence_is_malformed_not_verified():
    """AC8-ERR: absent proof cannot be interpreted as a successful continuation."""
    receipt = deepcopy(verified_receipt())
    del receipt["correlation_id"]

    result = load_diagnostic().classify_receipt(receipt)

    assert result["ok"] is False
    assert result["class"] == "malformed-output"
    assert result["failed_reader"] == "receipt.correlation_id"
