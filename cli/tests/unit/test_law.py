"""Proposal and single-use consent contracts for the chat law path."""

from __future__ import annotations

import json
from contextlib import contextmanager
from pathlib import Path

import pytest
from typer.testing import CliRunner


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


def test_operator_records_law_in_one_call_with_two_positionals(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _isolate(tmp_path, monkeypatch)
    from fno import decide, paths
    from fno.cli import app

    index = tmp_path / "state" / "decisions.jsonl"
    monkeypatch.setattr(paths, "decisions_jsonl", lambda: index)
    monkeypatch.setattr(decide, "_project", lambda _event: None)
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: type("Identity", (), {"session_id": None, "harness": None})(),
    )
    monkeypatch.setattr(decide, "_attended_terminal", lambda: True)

    recorded = CliRunner().invoke(
        app,
        ["law", "set", "worktree-policy", "Codex uses path-addressed worktrees"],
        catch_exceptions=False,
    )

    assert recorded.exit_code == 0, recorded.output
    decision_id = recorded.stdout.strip().splitlines()[-1]
    rows = [json.loads(line) for line in index.read_text().splitlines() if line]
    assert [row["data"]["decision_id"] for row in rows] == [decision_id]
    assert rows[0]["data"]["subject"] == "worktree-policy"
    assert rows[0]["data"]["decision"] == "Codex uses path-addressed worktrees"
    assert rows[0]["data"]["authority_source"] == "operator"


def test_agent_cannot_use_one_call_law_writer(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _isolate(tmp_path, monkeypatch)
    from fno.cli import app

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: type(
            "Identity",
            (),
            {"session_id": "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4", "harness": "codex"},
        )(),
    )

    refused = CliRunner().invoke(
        app,
        ["law", "set", "worktree-policy", "Codex uses path-addressed worktrees"],
        catch_exceptions=False,
    )

    assert refused.exit_code == 3, refused.output
    assert "fno backlog note" in refused.output


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
    tool_input = f"fno inbox law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}"
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
    tool_input = f"fno inbox law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}"
    armed = law._arm_proposal_from_hook(
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
        tool_input=armed["armed_tool_input"],
    )

    written = decide.record_decision(
        subject=proposal["subject"],
        decision=proposal["decision"],
        rationale=proposal["rationale"],
        authority_source="chat_attested",
        consent=consent,
        events_root=tmp_path,
    )

    assert written["event"]["data"]["authority_source"] == "chat_attested"
    assert law.load_proposal(proposal["proposal_id"])["status"] == "consumed"
    with pytest.raises(law.InvalidOperatorConsentError, match="consumed"):
        decide.record_decision(
            subject=proposal["subject"],
            decision=proposal["decision"],
            authority_source="chat_attested",
            consent=consent,
            events_root=tmp_path,
        )
    rows = [line for line in index.read_text(encoding="utf-8").splitlines() if line]
    assert len(rows) == 1


def test_enact_requires_a_hook_approval_receipt(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _isolate(tmp_path, monkeypatch)
    from fno import decide, law

    proposal = law.prepare_proposal(
        subject="x-12ba",
        decision="Merges belong to the operator",
        rationale="Durable policy needs human approval.",
    )
    tool_input = f"fno inbox law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}"
    law._arm_proposal_from_hook(
        proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="human-session-1",
        permission_mode="default",
        tool_input=tool_input,
    )
    monkeypatch.setattr(decide, "record_decision", lambda **_: {"decision_id": "d-12345678"})

    with pytest.raises(law.InvalidOperatorConsentError, match="approval receipt"):
        law.enact_proposal(proposal["proposal_id"], proposal["content_hash"])


def test_expired_consent_window_allows_rearming(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _isolate(tmp_path, monkeypatch)
    from fno import law

    proposal = law.prepare_proposal(
        subject="x-12ba",
        decision="Merges belong to the operator",
        rationale="Durable policy needs human approval.",
    )
    tool_input = f"fno inbox law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}"
    law._arm_proposal_from_hook(
        proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="abandoned-session",
        permission_mode="default",
        tool_input=tool_input,
    )
    stored = law.load_proposal(proposal["proposal_id"])
    stored["consent_expires_at"] = "2000-01-01T00:00:00+00:00"
    law.write_proposal(stored)

    rearmed = law._arm_proposal_from_hook(
        proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="new-session",
        permission_mode="default",
        tool_input=tool_input,
    )

    assert rearmed["status"] == "armed"
    assert rearmed["armed_session_id"] == "new-session"


def test_approval_receipt_is_not_persisted_in_proposal_state(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _isolate(tmp_path, monkeypatch)
    from fno import law

    proposal = law.prepare_proposal(
        subject="x-12ba",
        decision="Merges belong to the operator",
        rationale="Durable policy needs human approval.",
    )
    tool_input = f"fno inbox law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}"
    armed = law._arm_proposal_from_hook(
        proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="human-session-1",
        permission_mode="default",
        tool_input=tool_input,
    )
    stored = law.load_proposal(proposal["proposal_id"])

    assert "approval_receipt" in armed
    assert "approval_receipt" not in stored
    assert "armed_tool_input" not in stored


def test_proposal_arming_holds_the_proposal_lock(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _isolate(tmp_path, monkeypatch)
    from fno import law

    proposal = law.prepare_proposal(
        subject="x-12ba",
        decision="Merges belong to the operator",
        rationale="Durable policy needs human approval.",
    )
    calls: list[str] = []

    @contextmanager
    def lock(proposal_id: str):
        calls.append(proposal_id)
        yield

    monkeypatch.setattr(law, "proposal_lock", lock)
    law._arm_proposal_from_hook(
        proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="human-session-1",
        permission_mode="default",
        tool_input=f"fno inbox law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}",
    )

    assert calls == [proposal["proposal_id"]]


def test_consuming_proposal_cannot_be_rearmed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _isolate(tmp_path, monkeypatch)
    from fno import law

    proposal = law.prepare_proposal(
        subject="x-12ba",
        decision="Merges belong to the operator",
        rationale="Durable policy needs human approval.",
    )
    stored = law.load_proposal(proposal["proposal_id"])
    stored.update({"status": "consuming", "consuming_decision_id": "d-12345678"})
    law.write_proposal(stored)

    with pytest.raises(law.InvalidOperatorConsentError, match="consuming"):
        law._arm_proposal_from_hook(
            proposal["proposal_id"],
            content_hash=proposal["content_hash"],
            session_id="new-session",
            permission_mode="default",
            tool_input=f"fno inbox law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}",
        )


def test_enact_surfaces_index_recovery_receipt(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from typer.testing import CliRunner
    from fno import decide, law

    def fail(*args: object, **kwargs: object) -> dict[str, object]:
        raise decide.IndexWriteError("d-12345678", OSError("index unavailable"))

    monkeypatch.setattr(law, "enact_proposal", fail)
    result = CliRunner().invoke(
        law.law_app,
        [
            "enact",
            "--proposal",
            "lp-0123456789ab",
            "--hash",
            "a" * 64,
            "--receipt",
            "receipt-1",
        ],
    )

    assert result.exit_code == 4
    assert "d-12345678" in result.output
    assert "fno backlog decide-reindex" in result.output


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
    tool_input = f"fno inbox law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}"
    armed = law._arm_proposal_from_hook(
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
        "tool_input": armed["armed_tool_input"],
    }
    values[field] = value
    consent = decide.OperatorConsent(**values)

    with pytest.raises(law.InvalidOperatorConsentError, match=message):
        decide.record_decision(
            subject=proposal["subject"],
            decision=proposal["decision"],
            authority_source="chat_attested",
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
            tool_input="fno inbox law enact",
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
    tool_input = f"fno inbox law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}"
    armed = law._arm_proposal_from_hook(
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
        tool_input=armed["armed_tool_input"],
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
            authority_source="chat_attested",
            consent=consent,
            events_root=tmp_path,
        )
    assert law.load_proposal(proposal["proposal_id"])["status"] == "armed"
    assert not index.exists()
