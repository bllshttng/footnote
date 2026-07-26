"""Evidence-backed pre-design closure classification."""
from __future__ import annotations

from fno.graph.relatedness import classify_closure


SHA = "a" * 40


def probe(status: str, **overrides: object) -> dict[str, object]:
    value: dict[str, object] = {
        "status": status,
        "command": "fno test cli/tests/unit/test_login.py",
        "head": SHA,
        "observed": "login accepts a valid session",
    }
    value.update(overrides)
    return value


def merged(**overrides: object) -> dict[str, object]:
    value: dict[str, object] = {
        "status": "merged",
        "commit": SHA,
        "on_current_main": True,
        "observed": "login accepts a valid session",
    }
    value.update(overrides)
    return value


def test_passing_current_main_probe_proves_already_shipped() -> None:
    result = classify_closure(
        behavior="login accepts a valid session",
        current_main_probe=probe("passed"),
    )

    assert result["state"] == "already_shipped"
    assert result["proof"]["kind"] == "current_main_probe"
    assert result["proof"]["head"] == SHA
    assert result["proof"]["command"] == "fno test cli/tests/unit/test_login.py"


def test_reachable_merged_commit_proves_already_shipped() -> None:
    result = classify_closure(
        behavior="login accepts a valid session",
        merged_history=merged(),
    )

    assert result["state"] == "already_shipped"
    assert result["proof"]["kind"] == "merged_commit"
    assert result["proof"]["commit"] == SHA


def test_failed_current_main_probe_keeps_work_live_even_with_old_merge() -> None:
    result = classify_closure(
        behavior="login accepts a valid session",
        current_main_probe=probe("failed"),
        merged_history=merged(),
    )

    assert result["state"] == "live"
    assert result["proof"]["kind"] == "current_main_probe"


def test_uncertainty_never_closes_work() -> None:
    for status in ("unavailable", "error", "pending", "stale"):
        result = classify_closure(
            behavior="login accepts a valid session",
            current_main_probe=probe(status),
        )
        assert result["state"] == "unknown"


def test_malformed_or_unreachable_delivery_evidence_is_unknown() -> None:
    specimens = (
        merged(commit="short"),
        merged(on_current_main=False),
        merged(status="closed"),
        {"status": "merged", "title": "fix login"},
        {"status": "passed", "receipt": "preflight green"},
    )

    for evidence in specimens:
        result = classify_closure(
            behavior="login accepts a valid session",
            merged_history=evidence,
        )
        assert result["state"] == "unknown"


def test_positive_probe_must_bind_behavior_command_and_full_head() -> None:
    specimens = (
        probe("passed", command=""),
        probe("passed", head="short"),
        probe("passed", observed=""),
        probe("failed", head="short"),
    )

    for evidence in specimens:
        result = classify_closure(
            behavior="login accepts a valid session",
            current_main_probe=evidence,
        )
        assert result["state"] == "unknown"


def test_absent_behavior_is_unknown_even_with_positive_carrier_evidence() -> None:
    assert classify_closure(
        behavior="",
        current_main_probe=probe("passed"),
        merged_history=merged(),
    )["state"] == "unknown"
