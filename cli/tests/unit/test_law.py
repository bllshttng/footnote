"""Proposal and single-use consent contracts for the chat law path."""

from __future__ import annotations

import json
from pathlib import Path

import pytest


def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))
    from fno import paths

    paths.resolve_repo_root.cache_clear()
    (tmp_path / ".fno").mkdir(parents=True)


def test_prepare_normalizes_fields_and_lists_existing_law(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _isolate(tmp_path, monkeypatch)
    from fno import law, paths

    index = tmp_path / "state" / "decisions.jsonl"
    index.parent.mkdir()
    index.write_text(
        json.dumps(
            {
                "type": "operator_decision",
                "ts": "2026-08-24T19:00:00+00:00",
                "data": {
                    "decision_id": "d-existing1",
                    "subject": "x-12ba",
                    "decision": "Merges belong to the operator",
                    "authority_source": "operator",
                    "decided_by": "operator",
                },
            }
        )
        + "\n",
        encoding="utf-8",
    )
    monkeypatch.setattr(paths, "decisions_jsonl", lambda: index)

    proposal = law.prepare_proposal(
        subject=" x-12ba ",
        decision=" Merges belong to the operator ",
        rationale=" The operator owns durable policy. ",
        options=["operator", "agent"],
        supersedes=None,
    )

    assert proposal["status"] == "pending"
    assert proposal["subject"] == "x-12ba"
    assert proposal["decision"] == "Merges belong to the operator"
    assert proposal["existing_law_ids"] == ["d-existing1"]
    assert len(proposal["content_hash"]) == 64
    assert law.proposal_path(proposal["proposal_id"]).exists()


def test_prepare_rejects_coordination_wording(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _isolate(tmp_path, monkeypatch)
    from fno.law import ProposalValidationError, prepare_proposal

    with pytest.raises(ProposalValidationError, match="coordination"):
        prepare_proposal(
            subject="x-12ba",
            decision="For this PR only, use the temporary workaround",
            rationale="This expires at merge.",
        )


def test_armed_proposal_cannot_be_rebound_to_another_session(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _isolate(tmp_path, monkeypatch)
    from fno import law

    proposal = law.prepare_proposal(
        subject="x-12ba",
        decision="Merges belong to the operator",
        rationale="Durable policy needs human approval.",
    )
    tool_input = f"fno law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}"
    law._arm_proposal_from_hook(
        proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="human-session-1",
        permission_mode="default",
        tool_input=tool_input,
    )

    with pytest.raises(law.InvalidOperatorConsentError, match="already armed"):
        law._arm_proposal_from_hook(
            proposal["proposal_id"],
            content_hash=proposal["content_hash"],
            session_id="other-session",
            permission_mode="default",
            tool_input=tool_input,
        )


def test_valid_consent_records_once_and_replay_refuses_before_writes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _isolate(tmp_path, monkeypatch)
    from fno import decide, law, paths

    index = tmp_path / "state" / "decisions.jsonl"
    monkeypatch.setattr(paths, "decisions_jsonl", lambda: index)
    monkeypatch.setattr(decide, "_project", lambda _event: None)
    proposal = law.prepare_proposal(
        subject="x-12ba",
        decision="Merges belong to the operator",
        rationale="Durable policy needs human approval.",
    )
    tool_input = f"fno law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}"
    law._arm_proposal_from_hook(
        proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="human-session-1",
        permission_mode="default",
        tool_input=tool_input,
    )
    consent = decide.OperatorConsent(
        proposal_id=proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="human-session-1",
        permission_mode="default",
        tool_input=tool_input,
    )

    written = decide.record_decision(
        subject=proposal["subject"],
        decision=proposal["decision"],
        rationale=proposal["rationale"],
        authority_source="operator",
        consent=consent,
        events_root=tmp_path,
    )

    assert written["event"]["data"]["authority_source"] == "operator"
    assert law.load_proposal(proposal["proposal_id"])["status"] == "consumed"
    with pytest.raises(law.InvalidOperatorConsentError, match="consumed"):
        decide.record_decision(
            subject=proposal["subject"],
            decision=proposal["decision"],
            authority_source="operator",
            consent=consent,
            events_root=tmp_path,
        )
    rows = [line for line in index.read_text(encoding="utf-8").splitlines() if line]
    assert len(rows) == 1


@pytest.mark.parametrize(
    ("field", "value", "message"),
    [
        ("content_hash", "0" * 64, "content hash"),
        ("session_id", "other-session", "session"),
        ("permission_mode", "bypassPermissions", "permission"),
    ],
)
def test_mismatched_consent_refuses_before_any_decision_store(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    field: str,
    value: str,
    message: str,
) -> None:
    _isolate(tmp_path, monkeypatch)
    from fno import decide, law, paths

    index = tmp_path / "state" / "decisions.jsonl"
    monkeypatch.setattr(paths, "decisions_jsonl", lambda: index)
    monkeypatch.setattr(decide, "_project", lambda _event: None)
    proposal = law.prepare_proposal(
        subject="x-12ba",
        decision="Merges belong to the operator",
        rationale="Durable policy needs human approval.",
    )
    tool_input = f"fno law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}"
    law._arm_proposal_from_hook(
        proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="human-session-1",
        permission_mode="default",
        tool_input=tool_input,
    )
    values = {
        "proposal_id": proposal["proposal_id"],
        "content_hash": proposal["content_hash"],
        "session_id": "human-session-1",
        "permission_mode": "default",
        "tool_input": tool_input,
    }
    values[field] = value
    consent = decide.OperatorConsent(**values)

    with pytest.raises(law.InvalidOperatorConsentError, match=message):
        decide.record_decision(
            subject=proposal["subject"],
            decision=proposal["decision"],
            authority_source="operator",
            consent=consent,
            events_root=tmp_path,
        )
    assert not index.exists()
    assert not (tmp_path / ".fno" / "events.jsonl").exists()


def test_expired_consent_refuses_and_leaves_proposal_staged(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _isolate(tmp_path, monkeypatch)
    from fno import decide, law, paths

    index = tmp_path / "state" / "decisions.jsonl"
    monkeypatch.setattr(paths, "decisions_jsonl", lambda: index)
    monkeypatch.setattr(decide, "_project", lambda _event: None)
    proposal = law.prepare_proposal(
        subject="x-12ba",
        decision="Merges belong to the operator",
        rationale="Durable policy needs human approval.",
    )
    stored = law.load_proposal(proposal["proposal_id"])
    stored["expires_at"] = "2000-01-01T00:00:00+00:00"
    law.write_proposal(stored)

    with pytest.raises(law.InvalidOperatorConsentError, match="expired"):
        law._arm_proposal_from_hook(
            proposal["proposal_id"],
            content_hash=proposal["content_hash"],
            session_id="human-session-1",
            permission_mode="default",
            tool_input="fno law enact",
        )
    assert law.load_proposal(proposal["proposal_id"])["status"] == "expired"


def test_event_failure_leaves_valid_consent_retryable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _isolate(tmp_path, monkeypatch)
    from fno import decide, law, paths

    index = tmp_path / "state" / "decisions.jsonl"
    monkeypatch.setattr(paths, "decisions_jsonl", lambda: index)
    proposal = law.prepare_proposal(
        subject="x-12ba",
        decision="Merges belong to the operator",
        rationale="Durable policy needs human approval.",
    )
    tool_input = f"fno law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}"
    law._arm_proposal_from_hook(
        proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="human-session-1",
        permission_mode="default",
        tool_input=tool_input,
    )
    consent = decide.OperatorConsent(
        proposal_id=proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="human-session-1",
        permission_mode="default",
        tool_input=tool_input,
    )

    import fno.events

    def fail_append(*args: object, **kwargs: object) -> None:
        raise OSError("journal unavailable")

    monkeypatch.setattr(fno.events, "append_event", fail_append)
    with pytest.raises(OSError, match="journal unavailable"):
        decide.record_decision(
            subject=proposal["subject"],
            decision=proposal["decision"],
            rationale=proposal["rationale"],
            authority_source="operator",
            consent=consent,
            events_root=tmp_path,
        )
    assert law.load_proposal(proposal["proposal_id"])["status"] == "armed"
    assert not index.exists()
