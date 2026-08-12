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


class _Msg:
    """Minimal stand-in for a bus envelope, matching the attrs cmd_drain_self reads."""

    def __init__(self, *, id, from_, to, kind, ts, body):  # noqa: A002 -- test fixture
        self.id = id
        self.from_ = from_
        self.to = to
        self.kind = kind
        self.ts = ts
        self.body = body


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


# ---------------------------------------------------------------------------
# W1.1: a positive drain marker at the ack point, and the sweep that prefers it
# ---------------------------------------------------------------------------


def _drain_setup(monkeypatch, msg):
    """Wire cmd_drain_self to see exactly one unread message for cl-abcd1234."""
    from fno import harness_identity
    from fno.bus import cursor as cursor_mod

    class _Ident:
        harness = "claude"
        session_id = "abcd1234"

    monkeypatch.setattr(harness_identity, "resolve_harness_identity", lambda: _Ident())
    monkeypatch.setattr(harness_identity, "canonical_handle", lambda sid: "cl-abcd1234")
    monkeypatch.setattr(
        cursor_mod, "scan_unread", lambda h: [msg] if h == "cl-abcd1234" else []
    )
    return cursor_mod


def test_drain_emits_marker_with_msg_id(monkeypatch):
    """AC1-HP: draining one message emits agent_mail_drained at the ack point,
    carrying the msg_id, recipient, and sender so a sender-side join lands."""
    from fno.agents import events as events_mod
    from fno.mail import cli as mail_cli

    msg = _Msg(
        id="m-drain1", from_="alice", to="cl-abcd1234", kind="note",
        ts="2026-08-11T00:00:00Z", body="hello",
    )
    cursor_mod = _drain_setup(monkeypatch, msg)
    monkeypatch.setattr(cursor_mod, "advance_cursor", lambda h, mid: True)
    captured = _capture_events(monkeypatch)

    mail_cli.cmd_drain_self(json_out=False)

    markers = [e for e in captured if e["kind"] == events_mod.KIND_AGENT_MAIL_DRAINED]
    assert markers, "no agent_mail_drained marker was emitted at the ack point"
    assert markers[0]["msg_id"] == "m-drain1"
    assert markers[0]["recipient"] == "cl-abcd1234"
    assert markers[0]["sender"] == "alice"


def test_drain_marker_is_at_ack_not_print(monkeypatch):
    """The W1 trap: a crash at the ack boundary (advance_cursor raises) must
    leave NO marker. drain-self is inject-before-ack, so the message re-surfaces
    next wake; a marker written at print would claim delivery for a message that
    is about to be delivered again."""
    from fno.agents import events as events_mod
    from fno.mail import cli as mail_cli

    msg = _Msg(
        id="m-trap", from_="alice", to="cl-abcd1234", kind="note",
        ts="2026-08-11T00:00:00Z", body="hello",
    )
    cursor_mod = _drain_setup(monkeypatch, msg)

    def _crash(_h, _mid):
        raise OSError("crash at the ack boundary")

    monkeypatch.setattr(cursor_mod, "advance_cursor", _crash)
    captured = _capture_events(monkeypatch)

    with pytest.raises(OSError):
        mail_cli.cmd_drain_self(json_out=False)

    markers = [e for e in captured if e["kind"] == events_mod.KIND_AGENT_MAIL_DRAINED]
    assert not markers, (
        "a marker was emitted before the ack committed; a crash between print "
        "and ack would leave a false receipt for a message that re-surfaces"
    )


def test_drain_marker_emit_failure_does_not_fail_drain(monkeypatch):
    """AC9-ERR: a failed marker write is swallowed; the message is still printed
    and acked. The observability gap degrades to the cursor fallback, never to a
    delivery failure."""
    from fno.agents import events as events_mod
    from fno.mail import cli as mail_cli

    msg = _Msg(
        id="m-ac9", from_="alice", to="cl-abcd1234", kind="note",
        ts="2026-08-11T00:00:00Z", body="hello",
    )
    cursor_mod = _drain_setup(monkeypatch, msg)
    advances: list[tuple] = []
    monkeypatch.setattr(
        cursor_mod, "advance_cursor", lambda h, mid: advances.append((h, mid)) or True
    )

    def _boom(kind, *, path=None, **data):  # noqa: ARG001
        raise OSError("disk full")

    monkeypatch.setattr(events_mod, "emit", _boom)

    mail_cli.cmd_drain_self(json_out=False)  # must not raise
    assert advances, "the cursor never advanced because the marker emit crashed the drain"


def test_sweep_prefers_drain_marker_over_cursor(tmp_path, monkeypatch):
    """AC6-CON: a message past its ttl_at with an unadvanced cursor is NOT a
    stale dead letter once it has an agent_mail_drained receipt."""
    from fno.agents import events
    from fno.doctor import _drained_msg_ids, _stale_dead_letters
    from fno.inbox.store import DurableOwner, write_new_thread

    use_tmpdir(monkeypatch, tmp_path)
    handle = write_new_thread(
        "cl-abcd1234", "alice", "send", "hi", owner=DurableOwner.DEAD_LETTER.value
    )
    events.emit(
        events.KIND_AGENT_MAIL_DRAINED, msg_id=handle.thread_id,
        recipient="cl-abcd1234", address_form="cl-abcd1234", sender="alice",
    )
    # Positive control: the marker-reading instrument finds the id we planted.
    assert handle.thread_id in _drained_msg_ids()

    findings = _stale_dead_letters()
    assert not [f for f in findings if f.get("msg_id") == handle.thread_id], (
        "a message with a drain marker was escalated as a stale dead letter"
    )


def test_sweep_legacy_message_without_marker_still_escalates(tmp_path, monkeypatch):
    """AC7-CON: legacy mail written before the marker keeps escalating on the
    cursor heuristic, so the absence of a marker is never falsely treated as
    presence. Also the positive control that AC6's silence is the marker, not a
    broken sweep."""
    from fno.doctor import _drained_msg_ids, _stale_dead_letters
    from fno.inbox.store import DurableOwner, write_new_thread

    use_tmpdir(monkeypatch, tmp_path)
    handle = write_new_thread(
        "cl-abcd1234", "alice", "send", "hi", owner=DurableOwner.DEAD_LETTER.value
    )
    assert _drained_msg_ids() == set()  # no marker planted

    findings = _stale_dead_letters()
    assert [f for f in findings if f.get("msg_id") == handle.thread_id], (
        "a legacy message with no marker and an unadvanced cursor stopped escalating"
    )


def test_manual_ack_emits_drain_marker(tmp_path, monkeypatch):
    """The manual `fno mail ack` path consumes mail by advancing the cursor; it
    must emit agent_mail_drained too, or those messages leave unread with no
    terminal event -- the same accounting gap drain-self closes."""
    from fno.inbox.store import write_new_thread
    from fno.mail import cli as mail_cli

    use_tmpdir(monkeypatch, tmp_path)
    handle = write_new_thread("cl-abcd1234", "alice", "send", "hi")
    captured = _capture_events(monkeypatch)

    mail_cli.cmd_bus_ack(msg_id=handle.thread_id, name="cl-abcd1234")

    markers = [e for e in captured if e.get("kind") == "agent_mail_drained"]
    assert markers, "a manual ack advanced the cursor without emitting a receipt"
    assert markers[0]["msg_id"] == handle.thread_id
    assert markers[0]["reason"] == "acked"
