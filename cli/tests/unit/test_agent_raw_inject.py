"""Constructor + schema tests for the agent_raw_inject provenance event.

The audit-floor event for an UNWRAPPED injection (no <fno_mail> marker survives
in the recipient transcript, so x-f26c's greppability property moves to the
ledger). Covers the fno.events schema-system builder; the transport emit sites
(mail-inject binary, mux pane send) are exercised in their own test modules.
"""
from __future__ import annotations

from fno.events import agent_raw_inject, validate


def test_agent_raw_inject_constructor_minimal():
    event = agent_raw_inject(
        target_session="ses-9",
        payload="/code-review medium --fix",
        harness="claude",
        lane="control.sock",
    )
    validate(event)  # the kind is schema-known and required fields are present
    assert event["type"] == "agent_raw_inject"
    assert event["source"] == "target"
    assert event["data"]["target_session"] == "ses-9"
    assert event["data"]["payload"] == "/code-review medium --fix"
    assert event["data"]["harness"] == "claude"
    assert event["data"]["lane"] == "control.sock"
    assert "sender" not in event["data"], "optional fields omitted when absent"


def test_agent_raw_inject_constructor_carries_enrichment():
    # AC27: a self-injection records sender == target_session, so the ledger
    # identifies every self-injection permanently; AC34: target_cwd/target_head
    # carry the authorship-join facts (no computed verdict).
    event = agent_raw_inject(
        target_session="ses-9",
        payload="/compact",
        harness="codex",
        lane="codex-daemon",
        sender="ses-9",
        target_cwd="/repo",
        target_head="abc1234",
    )
    validate(event)
    assert event["data"]["sender"] == "ses-9"
    assert event["data"]["target_cwd"] == "/repo"
    assert event["data"]["target_head"] == "abc1234"
