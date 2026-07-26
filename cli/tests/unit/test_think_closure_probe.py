"""Evidence-backed pre-design closure classification."""
from __future__ import annotations

from pathlib import Path

import pytest

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


@pytest.fixture(autouse=True)
def resolved_main(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("fno.graph.relatedness._resolve_main_sha", lambda _repo: SHA)
    monkeypatch.setattr(
        "fno.graph.relatedness._commit_on_main", lambda _repo, commit, main: commit == SHA and main == SHA
    )


def classify(**kwargs: object) -> dict[str, object]:
    return classify_closure(repo=Path("/repo"), **kwargs)


def test_passing_current_main_probe_proves_already_shipped() -> None:
    result = classify(
        behavior="login accepts a valid session",
        current_main_probe=probe("passed"),
    )

    assert result["state"] == "already_shipped"
    assert result["proof"]["kind"] == "current_main_probe"
    assert result["proof"]["head"] == SHA
    assert result["proof"]["command"] == "fno test cli/tests/unit/test_login.py"


def test_reachable_merged_commit_proves_already_shipped() -> None:
    result = classify(
        behavior="login accepts a valid session",
        merged_history=merged(),
    )

    assert result["state"] == "already_shipped"
    assert result["proof"]["kind"] == "merged_commit"
    assert result["proof"]["commit"] == SHA


def test_failed_current_main_probe_keeps_work_live_even_with_old_merge() -> None:
    result = classify(
        behavior="login accepts a valid session",
        current_main_probe=probe("failed"),
        merged_history=merged(),
    )

    assert result["state"] == "live"
    assert result["proof"]["kind"] == "current_main_probe"


def test_uncertainty_never_closes_work() -> None:
    for status in ("unavailable", "error", "pending", "stale"):
        result = classify(
            behavior="login accepts a valid session",
            current_main_probe=probe(status),
        )
        assert result["state"] == "unknown"


def test_malformed_or_unreachable_delivery_evidence_is_unknown() -> None:
    specimens = (
        merged(commit="short"),
        merged(commit="b" * 40),
        merged(status="closed"),
        {"status": "merged", "title": "fix login"},
        {"status": "passed", "receipt": "preflight green"},
    )

    for evidence in specimens:
        result = classify(
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
        result = classify(
            behavior="login accepts a valid session",
            current_main_probe=evidence,
        )
        assert result["state"] == "unknown"


def test_absent_behavior_is_unknown_even_with_positive_carrier_evidence() -> None:
    assert classify(
        behavior="",
        current_main_probe=probe("passed"),
        merged_history=merged(),
    )["state"] == "unknown"


def test_arbitrary_full_sha_cannot_claim_current_main() -> None:
    result = classify(
        behavior="login accepts a valid session",
        current_main_probe=probe("passed", head="b" * 40),
    )

    assert result["state"] == "unknown"


def test_unresolved_main_fails_closed(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("fno.graph.relatedness._resolve_main_sha", lambda _repo: None)

    assert classify(
        behavior="login accepts a valid session",
        current_main_probe=probe("passed"),
    )["state"] == "unknown"
