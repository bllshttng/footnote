"""W1: close the delivery-accounting gap so a sender can join ``events.jsonl`` on
``msg_id`` to a terminal state for every send.

W1.2 (here): post-mint ``agent_send_failed`` emissions carry the minted
``msg_id``; pre-mint ones (``unknown-name``, ``lock-timeout``) carry none, so a
consumer joining on ``msg_id`` reads those as pre-mint failures rather than as
unaccounted messages (AC2-HP, AC3-ERR).

W1.1 (added alongside): ``cmd_drain_self`` emits ``agent_mail_drained`` at the
ack point and the dead-letter sweep prefers that marker over cursor inference.

Mocking strategy mirrors cli/tests/agents/test_send.py: ``use_tmpdir`` +
``write_registry`` for a deterministic registered peer, and the claude
live-inject seam defaulted to ``False`` so no test shells out to a real daemon.
Agent-dispatch events are captured by monkeypatching ``fno.agents.events.emit``
rather than reading a file, so the assertion is independent of where the log
lands.
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture(autouse=True)
def _no_real_mail_inject(monkeypatch):
    """Default the claude live-inject seam to 'not delivered' so no test shells
    out to a real fno-agents binary / daemon, and mark the peer family1-live so
    the live path is attempted (and fails) before the durable fallback."""
    from fno.agents import dispatch as dispatch_mod

    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", lambda recipient, text: False)
    monkeypatch.setattr(dispatch_mod, "_registered_family1_state", lambda _entry: "working")


def _register_claude_peer(name: str = "red") -> None:
    """One live claude AgentEntry with a full harness session id, so a send to it
    reaches the durable write (durable_recipient is set, clearing the
    durable-address gate)."""
    from fno.agents.registry import AgentEntry, write_registry

    write_registry([
        AgentEntry(
            name=name,
            harness="claude",
            harness_session_id="abcd1234-1111-7222-8333-444455556666",
            cwd="/tmp",
            log_path="/tmp/red.log",
            short_id="abcd1234",
            status="live",
        )
    ])


def _capture_events(monkeypatch) -> list[dict]:
    """Record every agent-dispatch event as a dict. ``emit_with_context``
    delegates to ``emit``, so patching ``emit`` captures both emit paths."""
    records: list[dict] = []
    from fno.agents import events as events_mod

    def _cap(kind, *, path=None, **data):  # noqa: ARG001 -- path is unused here
        records.append({"kind": kind, **data})

    monkeypatch.setattr(events_mod, "emit", _cap)
    return records


# ---------------------------------------------------------------------------
# W1.2: msg_id correlation on agent_send_failed
# ---------------------------------------------------------------------------


def test_send_failed_post_mint_carries_msg_id(runner, tmp_path, monkeypatch):
    """AC2-HP: a failure AFTER the id is minted (envelope-write) carries msg_id,
    so it joins to the agent_send_started record for the same send."""
    from fno.cli import app

    use_tmpdir(monkeypatch, tmp_path)
    _register_claude_peer("red")
    events = _capture_events(monkeypatch)

    def _boom(*_a, **_k):
        raise RuntimeError("disk on fire")

    monkeypatch.setattr("fno.inbox.store.write_new_thread", _boom)

    res = runner.invoke(app, ["mail", "send", "red", "hi", "--from-name", "web"])
    assert res.exit_code == 12, f"exit={res.exit_code} out={res.output!r}"

    failed = [e for e in events if e.get("kind") == "agent_send_failed"]
    assert failed, "no agent_send_failed event was emitted"
    env_write = [e for e in failed if e.get("stage") == "envelope-write"]
    assert env_write, (
        "the envelope-write failure stage did not fire; "
        f"stages seen: {[e.get('stage') for e in failed]}"
    )
    assert env_write[0].get("msg_id"), (
        "a post-mint failure carried no msg_id, leaving it uncorrelatable"
    )
    # The same id rides agent_send_started, so the join yields one send.
    started = [e for e in events if e.get("kind") == "agent_send_started"]
    assert started, "agent_send_started was not emitted"
    assert started[0].get("msg_id") == env_write[0].get("msg_id")


def test_send_failed_pre_mint_carries_no_msg_id(runner, tmp_path, monkeypatch):
    """AC3-ERR: unknown-name fires BEFORE the id is minted, so no msg_id field
    is invented. A consumer joining on msg_id skips these rather than treating
    them as unaccounted."""
    from fno.agents import discover
    from fno.cli import app

    use_tmpdir(monkeypatch, tmp_path)
    # Empty registry + blanked discovery: "nope-9f3c" resolves nowhere.
    empty = tmp_path / "empty-discovery"
    empty.mkdir()
    for env in (discover.SESSIONS_DIR_ENV, discover.PROJECTS_DIR_ENV, discover.CODEX_SESSIONS_DIR_ENV):
        monkeypatch.setenv(env, str(empty))
    events = _capture_events(monkeypatch)

    res = runner.invoke(app, ["mail", "send", "nope-9f3c", "hi", "--from-name", "web"])
    assert res.exit_code == 16, f"exit={res.exit_code} out={res.output!r}"

    failed = [e for e in events if e.get("kind") == "agent_send_failed"]
    assert failed, "no agent_send_failed event was emitted"
    assert all("msg_id" not in e for e in failed), (
        "a pre-mint failure carried a msg_id, inventing a key that matches nothing"
    )
