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

    monkeypatch.setattr(dispatch_mod, "_mail_inject_claude", lambda recipient, text, **_k: False)
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


# ---------------------------------------------------------------------------
# Class fix: cmd_notify_self is the third cursor-advancing mail path. It must
# emit drain markers and dedup against the transcript, like drain-self and ack.
# ---------------------------------------------------------------------------


class _StubStdout:
    """Captures cmd_notify_self's UserPromptSubmit JSON payload."""

    def __init__(self) -> None:
        self.written: list[str] = []

    def write(self, s: str) -> int:
        self.written.append(s)
        return len(s)

    def flush(self) -> None:
        pass

    def isatty(self) -> bool:
        return False


def _notify_setup(monkeypatch, msgs):
    """Wire cmd_notify_self to see ``msgs`` for cl-abcd1234, stub sent-unclaimed
    and config so the test exercises only the drain/dedup path."""
    from types import SimpleNamespace

    from fno import harness_identity
    from fno.bus import cursor as cursor_mod
    from fno.mail import cli as mail_cli

    class _Ident:
        harness = "claude"
        session_id = "abcd1234"

    monkeypatch.setattr(harness_identity, "resolve_harness_identity", lambda: _Ident())
    monkeypatch.setattr(harness_identity, "canonical_handle", lambda sid: "cl-abcd1234")
    monkeypatch.setattr(cursor_mod, "scan_unread", lambda h: msgs if h == "cl-abcd1234" else [])
    monkeypatch.setattr(mail_cli, "_sent_unclaimed", lambda *a, **k: [])
    monkeypatch.setattr(
        "fno.config.load_settings",
        lambda: SimpleNamespace(inbox=SimpleNamespace(unclaimed_ttl=3600)),
    )
    return cursor_mod


def test_notify_self_emits_markers_and_dedups(monkeypatch):
    """AC1-class: the push path receipts each message, printing fresh mail and
    skipping a duplicate whose id already landed in the transcript."""
    from fno.mail import cli as mail_cli

    dup = _Msg(id="n-dup", from_="alice", to="cl-abcd1234", kind="note",
               ts="2026-08-11T00:00:00Z", body="dup body")
    fresh = _Msg(id="n-fresh", from_="bob", to="cl-abcd1234", kind="note",
                 ts="2026-08-11T00:00:00Z", body="fresh body")
    cursor_mod = _notify_setup(monkeypatch, [dup, fresh])
    monkeypatch.setattr("fno.mail.reply_resolve.present_mail_ids", lambda: {"n-dup"})
    advances: list[tuple] = []
    monkeypatch.setattr(
        cursor_mod, "advance_cursor", lambda h, mid: advances.append((h, mid)) or True
    )
    captured = _capture_events(monkeypatch)
    out = _StubStdout()

    with monkeypatch.context() as patch:
        patch.setattr("sys.stdout", out)
        mail_cli.cmd_notify_self()

    rendered = "".join(out.written)
    assert "fresh body" in rendered, "a fresh message was not rendered"
    assert "dup body" not in rendered, "a duplicate was rendered a second time"
    assert advances, "the push path advanced no cursor"
    by_id = {m["msg_id"]: m["reason"] for m in captured if m.get("kind") == "agent_mail_drained"}
    assert by_id.get("n-dup") == "skipped-duplicate", "a duplicate was receipted as printed"
    assert by_id.get("n-fresh") == "printed", "a fresh message was receipted as a duplicate"


def test_notify_self_unreadable_transcript_prints_all(monkeypatch):
    """AC5-class: an unreadable transcript (present is None) falls through to
    printing every message. A read failure is not evidence of absence."""
    from fno.mail import cli as mail_cli

    msgs = [
        _Msg(id=f"n-{i}", from_="alice", to="cl-abcd1234", kind="note",
             ts="2026-08-11T00:00:00Z", body=f"body {i}")
        for i in range(2)
    ]
    cursor_mod = _notify_setup(monkeypatch, msgs)
    monkeypatch.setattr("fno.mail.reply_resolve.present_mail_ids", lambda: None)
    monkeypatch.setattr(cursor_mod, "advance_cursor", lambda h, mid: True)
    captured = _capture_events(monkeypatch)
    out = _StubStdout()

    with monkeypatch.context() as patch:
        patch.setattr("sys.stdout", out)
        mail_cli.cmd_notify_self()

    rendered = "".join(out.written)
    assert "body 0" in rendered and "body 1" in rendered, (
        "a message was skipped when the transcript could not be read"
    )
    reasons = {m["reason"] for m in captured if m.get("kind") == "agent_mail_drained"}
    assert reasons == {"printed"}, "an unreadable transcript receipted a message as a duplicate"


def test_notify_self_all_dups_advance_and_mark_silently(monkeypatch):
    """When every unread message is a duplicate, nothing is rendered but the
    cursor still advances and each gets a skipped-duplicate receipt."""
    from fno.mail import cli as mail_cli

    dup = _Msg(id="n-all", from_="alice", to="cl-abcd1234", kind="note",
               ts="2026-08-11T00:00:00Z", body="already seen")
    cursor_mod = _notify_setup(monkeypatch, [dup])
    monkeypatch.setattr("fno.mail.reply_resolve.present_mail_ids", lambda: {"n-all"})
    advances: list[tuple] = []
    monkeypatch.setattr(
        cursor_mod, "advance_cursor", lambda h, mid: advances.append((h, mid)) or True
    )
    captured = _capture_events(monkeypatch)
    out = _StubStdout()

    with monkeypatch.context() as patch:
        patch.setattr("sys.stdout", out)
        mail_cli.cmd_notify_self()

    assert not out.written, "a fully-duplicate mailbox rendered a payload"
    assert advances, "a fully-duplicate mailbox advanced no cursor"
    markers = [m for m in captured if m.get("kind") == "agent_mail_drained"]
    assert markers and markers[0]["reason"] == "skipped-duplicate"


def test_bus_ack_excludes_mail_arriving_after_snapshot(monkeypatch, tmp_path):
    """cmd_bus_ack builds its position snapshot from iter_messages, then reads
    scan_unread again. A message that lands between the two must NOT be receipted
    (the m.id-in-pos guard), or mail the user never acked gets a terminal event."""
    from fno.bus import cursor as cursor_mod
    from fno.bus import log as bus_log
    from fno.mail import cli as mail_cli

    use_tmpdir(monkeypatch, tmp_path)
    m_ack = _Msg(id="m-ack", from_="alice", to="fno", kind="note",
                 ts="2026-08-11T00:00:00Z", body="ack me")
    m_late = _Msg(id="m-late", from_="bob", to="fno", kind="note",
                  ts="2026-08-11T00:01:00Z", body="arrived after the snapshot")
    monkeypatch.setattr(bus_log, "iter_messages", lambda: [m_ack])
    monkeypatch.setattr(cursor_mod, "scan_unread", lambda n: [m_ack, m_late])
    monkeypatch.setattr(cursor_mod, "advance_cursor", lambda n, mid: True)
    captured = _capture_events(monkeypatch)

    mail_cli.cmd_bus_ack(msg_id="m-ack", name="fno")

    by_id = {m["msg_id"]: m for m in captured if m.get("kind") == "agent_mail_drained"}
    assert "m-ack" in by_id and by_id["m-ack"]["reason"] == "acked"
    assert "m-late" not in by_id, (
        "a message that arrived after the ack snapshot was receipted; the id-in-pos "
        "guard must exclude it"
    )


def test_drained_msg_ids_skips_non_dict_json(tmp_path, monkeypatch):
    """A valid-JSON non-object line carrying the marker substring must not crash
    the sweep; it is skipped and real markers are still read."""
    from fno.agents import events
    from fno.doctor import _drained_msg_ids
    from fno.paths import state_dir

    use_tmpdir(monkeypatch, tmp_path)
    events.emit(
        events.KIND_AGENT_MAIL_DRAINED, msg_id="m-real",
        recipient="cl-abcd1234", address_form="cl-abcd1234", sender="alice",
    )
    with (state_dir() / "events.jsonl").open("a", encoding="utf-8") as fh:
        fh.write('["agent_mail_drained"]\n')

    ids = _drained_msg_ids()
    assert "m-real" in ids, "a non-dict line suppressed a real marker"
