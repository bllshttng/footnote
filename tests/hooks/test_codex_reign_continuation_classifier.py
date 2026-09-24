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
        "schema_version": 3,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "versions": {"codex": "codex-cli 0.155.1", "fno": "0.3.2", "fno_agents": "0.3.2"},
        "session": {
            "id": "01a00000-0000-7000-8000-000000000001",
            "harness": "codex",
            "turn_ids": ["turn-0001", "turn-0002", "turn-0003"],
            "identity_constant": True,
            "scope": "disposable",
        },
        "correlation_id": "stop:01a00000-0000-7000-8000-000000000001:turn-0001",
        "continuation_owner": "goal",
        "action_hash": "sha256:nonce-action-0001",
        "user_message_count": 1,
        "command_requests": [
            {"method": "thread/goal/get", "status": "absent", "thread_id": "01a00000-0000-7000-8000-000000000001"},
            {"method": "thread/goal/get", "status": "verified", "thread_id": "01a00000-0000-7000-8000-000000000001"},
            {"method": "thread/compact/start", "status": "verified", "thread_id": "01a00000-0000-7000-8000-000000000001"},
            {"method": "king_goal_resumed", "status": "verified", "thread_id": "01a00000-0000-7000-8000-000000000001"},
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
            "scope": "disposable",
            "before": {"status": "absent", "objective": None, "usage": None},
            "before_receipt": "Codex provider goal unreadable: no goal",
            "init": {
                "ensure_receipt": {
                    "provider": "codex",
                    "thread_id": "01a00000-0000-7000-8000-000000000001",
                    "scope": "disposable",
                    "objective": "$fno:reign disposable",
                    "status": "active",
                    "continuation_owner": "king:disposable",
                    "usage": {
                        "token_budget": 50_000,
                        "tokens_used": 1,
                        "time_used_seconds": 1,
                    },
                },
                "ensure_completed_at_ns": 100,
                "manifest_written_at_ns": 200,
                "manifest": {
                    "written": True,
                    "scope": "disposable",
                    "thread_id": "01a00000-0000-7000-8000-000000000001",
                },
                "refused_retry": {"status": "refused", "manifest_unchanged": True},
                "refused_no_goal": {
                    "status": "refused",
                    "thread_id": "01a00000-0000-7000-8000-000000000001",
                    "scope": "disposable-refusal",
                    "provider_goal_readable": False,
                    "manifest_written": False,
                },
            },
            "after": {
                "status": "active",
                "objective": "$fno:reign disposable",
                "usage": {
                    "token_budget": 50_000,
                    "tokens_used": 1,
                    "time_used_seconds": 1,
                },
                "thread_id": "01a00000-0000-7000-8000-000000000001",
            },
            "paused": {
                "status": "paused",
                "objective": "$fno:reign disposable",
                "thread_id": "01a00000-0000-7000-8000-000000000001",
                "usage": {
                    "token_budget": 50_000,
                    "tokens_used": 1,
                    "time_used_seconds": 2,
                },
            },
            "resumed": {
                "status": "active",
                "objective": "$fno:reign disposable",
                "thread_id": "01a00000-0000-7000-8000-000000000001",
                "usage": {
                    "token_budget": 50_000,
                    "tokens_used": 1,
                    "time_used_seconds": 3,
                },
            },
        },
        "stop": {
            "independent": {
                "decision": "allow",
                "class": "visitor",
                "continuation_owner": "none",
                "session_id": "01a00000-0000-7000-8000-000000000001",
                "turn_id": "turn-0001",
                "correlation_id": "stop:01a00000-0000-7000-8000-000000000001:turn-0001",
                "goal_before": "absent",
                "useful_action_after_stop": True,
                "action_order": ["stop-visitor", "goal-init", "goal-useful-action"],
                "first_step_at_ns": 25,
                "visitor_at_ns": 50,
                "goal_ensured_at_ns": 100,
                "manifest_written_at_ns": 200,
                "useful_action_at_ns": 300,
            },
            "delegated": {
                "decision": "allow",
                "class": "delegated-to-goal",
                "continuation_owner": "goal",
                "session_id": "01a00000-0000-7000-8000-000000000001",
                "turn_id": "turn-0002",
                "correlation_id": "stop:01a00000-0000-7000-8000-000000000001:turn-0002",
                "block_count": 0,
                "useful_action": True,
                "useful_action_at_ns": 300,
                "stop_at_ns": 350,
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
                "session_id": "01a00000-0000-7000-8000-000000000001",
                "scope": "disposable",
                "park_count": 1,
                "stop_samples_during_hold": 0,
                "turns_during_hold": 0,
                "goal_usage_stable": True,
                "paused_goal_receipt": {
                    "thread_id": "01a00000-0000-7000-8000-000000000001",
                    "status": "paused",
                    "usage": {"token_budget": 50_000, "tokens_used": 1, "time_used_seconds": 2},
                },
                "held_goal_receipt": {
                    "thread_id": "01a00000-0000-7000-8000-000000000001",
                    "status": "paused",
                    "usage": {"token_budget": 50_000, "tokens_used": 1, "time_used_seconds": 2},
                },
                "park_interval_seconds": 0.25,
                "wake_result": "resumed",
            },
            "wake_receipt": {
                "session_id": "01a00000-0000-7000-8000-000000000001",
                "scope": "disposable",
                "reason": "board",
                "provider_receipt": {
                    "thread_id": "01a00000-0000-7000-8000-000000000001",
                    "status": "active",
                    "objective": "$fno:reign disposable",
                },
            },
            "compaction_receipt": {
                "verified": True,
                "provider": "codex",
                "action": "compact",
                "thread_id": "01a00000-0000-7000-8000-000000000001",
            },
            "private_daemon_replacement": {
                "command": "codex app-server daemon restart",
                "status": "verified",
                "session_id": "01a00000-0000-7000-8000-000000000001",
                "returncode": 0,
                "code_home_is_private": True,
            },
            "repeats": [
                {
                    "boundary": "compaction",
                    "status": "verified",
                    "same_session": True,
                    "useful_action": True,
                    "session_id": "01a00000-0000-7000-8000-000000000001",
                    "turn_id": "turn-0002",
                    "correlation_id": "stop:01a00000-0000-7000-8000-000000000001:turn-0002",
                    "action_hash": "sha256:nonce-action-0001",
                },
                {
                    "boundary": "resume",
                    "status": "verified",
                    "same_session": True,
                    "useful_action": True,
                    "session_id": "01a00000-0000-7000-8000-000000000001",
                    "turn_id": "turn-0003",
                    "correlation_id": "stop:01a00000-0000-7000-8000-000000000001:turn-0003",
                    "action_hash": "sha256:nonce-action-0001",
                },
                {
                    "boundary": "private-daemon-replacement",
                    "status": "verified",
                    "same_session": True,
                    "useful_action": True,
                    "session_id": "01a00000-0000-7000-8000-000000000001",
                    "turn_id": "turn-0003",
                    "correlation_id": "stop:01a00000-0000-7000-8000-000000000001:turn-0003",
                    "action_hash": "sha256:nonce-action-0001",
                },
            ],
            "provider_goal_refusal": {
                "thread_id": "01a00000-0000-7000-8000-000000000001",
                "scope": "disposable-refusal",
                "provider_goal_readable": False,
                "provider_error": "Codex app-server could not resume the exact thread",
                "init_error": "provider_goal: thread unreadable",
                "manifest_absent": True,
            },
        },
        "status": "verified",
    }


def test_ac10_hp_and_ac11_hp_require_independent_stop_and_single_user_message():
    """AC10-HP/AC11-HP: both continuation owners are proven without input injection."""
    result = load_diagnostic().classify_receipt(verified_receipt())

    assert result["ok"] is True
    assert result["class"] == "verified-continuation"
    assert result["failed_reader"] is None


def test_ac10_hp_accepts_visitor_stop_followed_by_goal_owned_continuation():
    """Crown init may ensure the goal after a visitor Stop on the same thread."""
    receipt = verified_receipt()
    receipt["stop"]["independent"].update(
        {
            "decision": "allow",
            "class": "visitor",
            "useful_action_after_stop": True,
            "action_order": ["stop-visitor", "goal-init", "goal-useful-action"],
        }
    )

    result = load_diagnostic().classify_receipt(receipt)

    assert result["ok"] is True
    assert result["class"] == "verified-continuation"


def test_ac11_hp_rejects_a_manifest_written_before_provider_goal_ensure():
    receipt = verified_receipt()
    receipt["goal"]["init"]["manifest_written_at_ns"] = 99

    result = load_diagnostic().classify_receipt(receipt)

    assert result == {
        "ok": False,
        "class": "malformed-output",
        "failed_reader": "goal.init.order",
    }


def test_ac11_edge_rejects_a_retry_that_replaces_the_verified_crown():
    receipt = verified_receipt()
    receipt["goal"]["init"]["refused_retry"]["manifest_unchanged"] = False

    result = load_diagnostic().classify_receipt(receipt)

    assert result == {
        "ok": False,
        "class": "identity-miss",
        "failed_reader": "goal.init.refused_retry",
    }


def test_ac11_edge_rejects_unreadable_goal_init_that_writes_a_manifest():
    receipt = verified_receipt()
    receipt["goal"]["init"]["refused_no_goal"]["manifest_written"] = True

    result = load_diagnostic().classify_receipt(receipt)

    assert result == {
        "ok": False,
        "class": "identity-miss",
        "failed_reader": "goal.init.refused_no_goal",
    }


def test_ac10_edge_requires_the_exact_board_wake_and_private_daemon_receipts():
    receipt = verified_receipt()
    receipt["proof"]["wake_receipt"]["provider_receipt"]["thread_id"] = (
        "01a00000-0000-7000-8000-000000000002"
    )

    result = load_diagnostic().classify_receipt(receipt)

    assert result == {
        "ok": False,
        "class": "wake-refused",
        "failed_reader": "quiet_park.wake_receipt",
    }

    receipt = verified_receipt()
    receipt["proof"]["private_daemon_replacement"]["code_home_is_private"] = False

    result = load_diagnostic().classify_receipt(receipt)

    assert result == {
        "ok": False,
        "class": "resume-marker-stale",
        "failed_reader": "daemon.private_replacement",
    }


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


def test_ac11_park_rejects_reset_goal_usage():
    receipt = verified_receipt()
    receipt["goal"]["paused"]["usage"]["tokens_used"] = 0

    result = load_diagnostic().classify_receipt(receipt)

    assert result == {
        "ok": False,
        "class": "explicit-park",
        "failed_reader": "goal.usage",
    }


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


def test_previous_receipt_schema_is_not_accepted_as_current_live_evidence():
    receipt = verified_receipt()
    receipt["schema_version"] = 2

    result = load_diagnostic().classify_receipt(receipt)

    assert result == {
        "ok": False,
        "class": "malformed-output",
        "failed_reader": "receipt.schema_version",
    }


def test_provider_receipt_requires_verified_action_and_exact_thread():
    diagnostic = load_diagnostic()
    require_receipt = getattr(diagnostic, "_require_provider_receipt", None)
    assert callable(require_receipt), "smoke runner must validate native provider receipts"

    session_id = "01a00000-0000-7000-8000-000000000001"
    receipt = {
        "verified": True,
        "action": "goal_set",
        "provider": "codex",
        "thread_id": session_id,
        "status": "active",
        "objective": "$fno:reign disposable",
        "continuation_owner": "king:disposable",
        "usage": {"token_budget": 50000, "tokens_used": 1, "time_used_seconds": 1},
    }

    assert require_receipt(receipt, session_id=session_id, action="goal_set") == receipt

    wrong_thread = deepcopy(receipt)
    wrong_thread["thread_id"] = "01a00000-0000-7000-8000-000000000002"
    with pytest.raises(RuntimeError, match="identity-miss"):
        require_receipt(wrong_thread, session_id=session_id, action="goal_set")
