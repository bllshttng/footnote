"""End-to-end proposal journey with mail-shaped negative and approval positive controls."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).parents[3]
GATE_PATH = REPO_ROOT / "hooks" / "law-authority-gate.py"


def _gate():
    spec = importlib.util.spec_from_file_location("law_authority_gate_journey", GATE_PATH)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


@pytest.fixture
def isolated(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> tuple[Path, Path]:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))
    (tmp_path / ".fno").mkdir()
    from fno import decide, paths

    paths.resolve_repo_root.cache_clear()
    index = tmp_path / "state" / "decisions.jsonl"
    monkeypatch.setattr(paths, "decisions_jsonl", lambda: index)
    monkeypatch.setattr(decide, "_project", lambda _event: None)
    return tmp_path, index


def test_mail_shaped_turn_only_asks_and_leaves_stores_untouched(isolated) -> None:
    root, index = isolated
    from fno import law

    proposal = law.prepare_proposal(
        subject="x-12ba",
        decision="Merges belong to the operator",
        rationale="The operator owns durable policy.",
    )
    gate = _gate()
    result = gate.evaluate(
        {
            "tool_name": "Bash",
            "tool_input": {
                "command": f"fno inbox law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}"
            },
            "session_id": "mail-session",
            "permission_mode": "default",
        },
        arm=lambda **kwargs: law._arm_proposal_from_hook(
            kwargs["proposal_id"],
            content_hash=kwargs["content_hash"],
            session_id=kwargs["session_id"],
            permission_mode=kwargs["permission_mode"],
            tool_input=kwargs["tool_input"],
        ),
    )

    assert result["hookSpecificOutput"]["permissionDecision"] == "ask"
    assert law.load_proposal(proposal["proposal_id"])["status"] == "armed"
    assert not index.exists()
    assert not (root / ".fno" / "events.jsonl").exists()


def test_exact_approval_writes_two_stores_and_replay_is_refused(isolated) -> None:
    root, index = isolated
    from fno import decide, law

    proposal = law.prepare_proposal(
        subject="x-12ba",
        decision="Merges belong to the operator",
        rationale="The operator owns durable policy.",
    )
    tool_input = f"fno inbox law enact --proposal {proposal['proposal_id']} --hash {proposal['content_hash']}"
    armed = law._arm_proposal_from_hook(
        proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="human-session",
        permission_mode="default",
        tool_input=tool_input,
    )
    consent = decide.OperatorConsent(
        proposal_id=proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="human-session",
        permission_mode="default",
        tool_input=armed["armed_tool_input"],
    )
    recorded = decide.record_decision(
        subject=proposal["subject"],
        decision=proposal["decision"],
        rationale=proposal["rationale"],
        authority_source="chat_attested",
        consent=consent,
        events_root=root,
    )
    decision_id = recorded["decision_id"]

    events = [
        json.loads(line)
        for line in (root / ".fno" / "events.jsonl").read_text(encoding="utf-8").splitlines()
        if line
    ]
    index_rows = [json.loads(line) for line in index.read_text(encoding="utf-8").splitlines() if line]
    assert [event["data"]["decision_id"] for event in events] == [decision_id]
    assert [row["data"]["decision_id"] for row in index_rows] == [decision_id]
    # A post-cutover chat approval is the law skill's internal authority proof.
    # It is not a user-typeable --authority value, so it reaches law without
    # reopening the agent-authored decision path.
    _, rows, _ = decide.list_decisions("x-12ba", limit=None, lane="law")
    assert [row["decision_id"] for row in rows] == [decision_id]

    with pytest.raises(law.InvalidOperatorConsentError, match="consumed"):
        decide.record_decision(
            subject=proposal["subject"],
            decision=proposal["decision"],
            authority_source="chat_attested",
            consent=consent,
            events_root=root,
        )
    assert len(index.read_text(encoding="utf-8").splitlines()) == 1


def test_chat_approval_can_supersede_existing_law(isolated) -> None:
    root, index = isolated
    from fno import decide, law

    existing_id = "d-12345678"
    index.parent.mkdir(parents=True, exist_ok=True)
    index.write_text(
        json.dumps(
            {
                "type": "operator_decision",
                "ts": "2026-08-25T23:00:00Z",
                "data": {
                    "decision_id": existing_id,
                    "subject": "x-12ba",
                    "decision": "Old law",
                    "decided_by": "operator",
                    "authority_source": "operator",
                },
            }
        )
        + "\n",
        encoding="utf-8",
    )
    proposal = law.prepare_proposal(
        subject="x-12ba",
        decision="New law",
        rationale="The operator replaced the old law.",
        supersedes=existing_id,
    )
    tool_input = (
        f"fno law enact --proposal {proposal['proposal_id']} "
        f"--hash {proposal['content_hash']}"
    )
    armed = law._arm_proposal_from_hook(
        proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="human-session",
        permission_mode="default",
        tool_input=tool_input,
    )
    consent = decide.OperatorConsent(
        proposal_id=proposal["proposal_id"],
        content_hash=proposal["content_hash"],
        session_id="human-session",
        permission_mode="default",
        tool_input=armed["armed_tool_input"],
    )

    recorded = decide.record_decision(
        subject=proposal["subject"],
        decision=proposal["decision"],
        rationale=proposal["rationale"],
        supersedes=existing_id,
        authority_source="chat_attested",
        consent=consent,
        events_root=root,
    )

    assert recorded["event"]["data"]["supersedes"] == existing_id
    _, history, _ = decide.list_decisions("x-12ba", state="all")
    by_id = {row["decision_id"]: row for row in history}
    assert by_id[existing_id]["superseded_by"] == recorded["decision_id"]
