"""US8 (x-f07d): send-time human escalation for `--kind question`.

A `--kind question` send notifies the human at send time (Locked Decision 7:
question NEVER autonomous-responds), debounced per (sender, recipient) so a
chatty peer cannot spam the human's queue. The durable question thread is always
written, so the ambient unread count stays truthful even when the notifier is
debounced.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

import fno.events
from fno.cli import app
from fno.paths_testing import use_tmpdir


@pytest.fixture
def runner():
    return CliRunner()


@pytest.fixture
def mailbox(tmp_path, monkeypatch):
    monkeypatch.delenv("FNO_BUS_DIR", raising=False)
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "daemon-empty"))
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    use_tmpdir(monkeypatch, tmp_path)
    return tmp_path


@pytest.fixture
def notified(monkeypatch):
    """Capture every send_notification call (best-effort OS notifier)."""
    calls: list[tuple[str, str]] = []

    def fake(title: str, message: str):
        calls.append((title, message))
        return (0, "")

    monkeypatch.setattr("fno.notify._impl.send_notification", fake)
    return calls


@pytest.fixture
def emitted_events(monkeypatch):
    """Capture the mail_escalation events the escalation helper emits.

    Replaces the project events.jsonl write with an in-memory capture so the
    test asserts emission + schema-validity without depending on the
    cwd-relative events.jsonl path. The builder still runs real (it validates
    via _build), so a captured event is exactly what would have been appended.
    """
    events: list[dict] = []

    def capture(event: dict) -> None:
        events.append(event)

    monkeypatch.setattr("fno.events.append_event", capture)
    return events


def _unread_count(runner, name: str) -> int:
    res = runner.invoke(app, ["mail", "unread", "--name", name, "--json"])
    assert res.exit_code == 0, res.output
    return len(json.loads(res.stdout.strip().splitlines()[-1]))


def test_question_send_escalates_to_human(runner, mailbox, notified):
    res = runner.invoke(
        app,
        ["mail", "send", "--to-project", "web", "--kind", "question",
         "--from-name", "etl", "--body", "which schema wins?"],
    )
    assert res.exit_code == 0, res.output
    assert len(notified) == 1, "one escalation fires at send time"
    title, message = notified[0]
    assert "etl" in (title + message)
    # The durable question is written regardless (truthful unread count).
    assert _unread_count(runner, "web") == 1


def test_question_escalation_debounced_per_pair(runner, mailbox, notified):
    for _ in range(3):
        res = runner.invoke(
            app,
            ["mail", "send", "--to-project", "web", "--kind", "question",
             "--from-name", "etl", "--body", "spam?"],
        )
        assert res.exit_code == 0, res.output
    # A chatty peer escalates ONCE (debounced) but every question is still queued.
    assert len(notified) == 1
    assert _unread_count(runner, "web") == 3


def test_debounce_is_per_sender_recipient_pair(runner, mailbox, notified):
    runner.invoke(
        app,
        ["mail", "send", "--to-project", "web", "--kind", "question",
         "--from-name", "etl", "--body", "q1"],
    )
    runner.invoke(
        app,
        ["mail", "send", "--to-project", "web", "--kind", "question",
         "--from-name", "ops", "--body", "q2"],
    )
    # Distinct senders are distinct pairs: both escalate.
    assert len(notified) == 2


def test_question_send_emits_one_valid_overlay_event(runner, mailbox, emitted_events):
    # The escalation surfaces in the needs-me overlay via a mail_escalation event
    # (US1/US2): exactly one per non-debounced escalation, schema-valid.
    res = runner.invoke(
        app,
        ["mail", "send", "--to-project", "web", "--kind", "question",
         "--from-name", "etl", "--body", "which schema wins?"],
    )
    assert res.exit_code == 0, res.output
    assert len(emitted_events) == 1, "exactly one mail_escalation event per escalation"
    ev = emitted_events[0]
    assert ev["type"] == "mail_escalation"
    fno.events.validate(ev)  # raises if the envelope/shape is invalid
    d = ev["data"]
    assert d["reason"] == "question"
    assert d["sender"] == "etl"
    assert d["recipient"] == "web"
    assert "which schema wins?" in d["summary"]
    assert d["msg_id"].startswith("msg-"), "carries the mail id for correlation"


def test_debounce_gates_the_event_exactly_like_the_notifier(
    runner, mailbox, notified, emitted_events
):
    # A chatty pair escalates once: one notifier call AND one event (AC6-FR).
    for _ in range(3):
        res = runner.invoke(
            app,
            ["mail", "send", "--to-project", "web", "--kind", "question",
             "--from-name", "etl", "--body", "spam?"],
        )
        assert res.exit_code == 0, res.output
    assert len(notified) == 1
    assert len(emitted_events) == 1, "debounce gates the event exactly as the notifier"


def test_headsup_send_wakes_asleep_claude_addressee(runner, mailbox, monkeypatch):
    # US9 P1: the per-project watch daemon never drains a handle inbox, so a
    # heads-up to a resumable-but-asleep claude handle is woken at send time.
    from fno.agents.discover import ReachableSession

    monkeypatch.setattr(
        "fno.agents.discover.resolve_reachable",
        lambda t, **k: (ReachableSession(session_id="sess-uuid-1", source="transcript", agent="claude"), []),
    )
    calls: list[str] = []
    monkeypatch.setattr(
        "fno.agents.dispatch.wake_drain_agent",
        lambda sid, **k: calls.append(sid) or (True, "wake-sessuui"),
    )
    res = runner.invoke(
        app,
        ["mail", "send", "--to-project", "peer", "--kind", "heads-up",
         "--from-name", "bob", "--body", "PR merged, take a look"],
    )
    assert res.exit_code == 0, res.output
    assert calls == ["sess-uuid-1"], "a heads-up to an asleep claude handle wakes it at send time"
    assert _unread_count(runner, "peer") == 1  # durable note is still written


def test_fyi_and_heads_up_do_not_escalate(runner, mailbox, notified):
    for kind in ("fyi", "heads-up"):
        runner.invoke(
            app,
            ["mail", "send", "--to-project", "web", "--kind", kind,
             "--from-name", "etl", "--body", f"a {kind}"],
        )
    assert notified == [], "only question escalates to the human"


def test_notifier_unavailable_does_not_claim_escalation(runner, mailbox, monkeypatch):
    # A headless host: send_notification returns (1, err) rather than raising.
    monkeypatch.setattr(
        "fno.notify._impl.send_notification", lambda t, m: (1, "no notifier")
    )
    res = runner.invoke(
        app,
        ["mail", "send", "--to-project", "web", "--kind", "question",
         "--from-name", "etl", "--body", "q"],
    )
    assert res.exit_code == 0, res.output
    assert "escalated to human" not in res.output, "no false claim when nothing displayed"
    assert _unread_count(runner, "web") == 1


def test_escalation_failure_never_breaks_the_send(runner, mailbox, monkeypatch):
    def boom(title: str, message: str):
        raise RuntimeError("no display")

    monkeypatch.setattr("fno.notify._impl.send_notification", boom)
    res = runner.invoke(
        app,
        ["mail", "send", "--to-project", "web", "--kind", "question",
         "--from-name", "etl", "--body", "still delivers"],
    )
    assert res.exit_code == 0, res.output
    assert _unread_count(runner, "web") == 1


# ---------------------------------------------------------------------------
# Attended-recipient live-miss lane (US3): a send to an operator-attended
# session that misses live delivery escalates; a worker or a live-confirmed
# send does not. Drives _name_lane_send directly with a constructed resolved
# session + mocked injectors, so the assertion is the escalation site itself.
# ---------------------------------------------------------------------------


def _resolved_claude(session_id: str):
    """A minimal duck-typed resolved session for _name_lane_send."""
    from types import SimpleNamespace

    return SimpleNamespace(
        session_id=session_id, agent="claude", handle=session_id[-8:]
    )


def _skip_mux(monkeypatch):
    """Force the mux-pane rung to skip (resolve_agent raises) so a live-miss
    falls straight to the durable floor."""
    from fno.agents.registry import AgentResolutionError

    def _raise(*_a, **_k):
        raise AgentResolutionError("test: skip mux rung")

    monkeypatch.setattr("fno.agents.registry.resolve_agent", _raise)


def test_attended_live_miss_escalates(mailbox, monkeypatch, emitted_events):
    from fno.agents.registry import register_existing_session
    from fno.mail.cli import _name_lane_send

    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    register_existing_session(
        provider="claude", session_id=sid, cwd=str(mailbox), origin="operator"
    )
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: False)
    _skip_mux(monkeypatch)

    _name_lane_send("need your eyes on this", from_name="sender", resolved=_resolved_claude(sid))

    assert len(emitted_events) == 1, "operator live-miss escalates once"
    assert emitted_events[0]["data"]["reason"] == "attended-miss"
    assert emitted_events[0]["data"]["recipient"] == "9a063cd3"


def test_reachable_worker_live_miss_escalates_as_reachable_miss(
    mailbox, monkeypatch, emitted_events
):
    # A spawn/host worker row: no origin -> not attended, so it never matched the
    # attended lane and a live miss sat silent. The reachable-miss lane covers it
    # (node x-1904). The overlay event must actually LAND: its reason is an
    # enforced enum, and the emit is best-effort, so an unlisted reason would be
    # swallowed and this lane would surface nothing on a headless host.
    from fno.agents.registry import register_existing_session
    from fno.mail.cli import _name_lane_send

    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    register_existing_session(provider="claude", session_id=sid, cwd=str(mailbox))
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: False)
    _skip_mux(monkeypatch)

    _name_lane_send("fyi", from_name="sender", resolved=_resolved_claude(sid))

    assert len(emitted_events) == 1, "a reachable worker live-miss escalates once"
    assert emitted_events[0]["data"]["reason"] == "reachable-miss"


def test_live_confirmed_send_does_not_escalate(mailbox, monkeypatch, emitted_events):
    from fno.agents.registry import register_existing_session
    from fno.mail.cli import _name_lane_send

    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    register_existing_session(
        provider="claude", session_id=sid, cwd=str(mailbox), origin="operator"
    )
    # Live inject succeeds -> delivered (hosted); the durable block (and the
    # attended-miss site in it) never runs.
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_claude", lambda *_a, **_k: True)

    _name_lane_send("already in your transcript", from_name="sender", resolved=_resolved_claude(sid))

    assert emitted_events == [], "a live-confirmed inject is in front of the human already"


def test_registry_read_failure_escalates_neither_nor_breaks(monkeypatch):
    # An unreadable registry must read as not-attended (never raise), so the
    # send still succeeds and escalates nothing (AC3-ERR).
    from fno.mail.cli import _recipient_is_attended

    def _boom(*_a, **_k):
        raise OSError("registry unreadable")

    monkeypatch.setattr("fno.agents.registry.load_registry", _boom)
    assert _recipient_is_attended("9a063cd3") is False


# ---------------------------------------------------------------------------
# AC8 (x-c24d Wave 1): a codex send that demotes to durable with no app-server
# daemon prints the fix command, not only the live-miss reason token. The
# codex daemon socket is the live-mail prerequisite; naming `codex app-server
# daemon start` at the demote site is self-teaching runtime text.
# ---------------------------------------------------------------------------


def _resolved_codex(session_id: str):
    """A minimal duck-typed resolved codex session for _name_lane_send."""
    from types import SimpleNamespace

    return SimpleNamespace(
        session_id=session_id, agent="codex", handle=session_id[-8:]
    )


def test_mail_demote_reason_codex_no_daemon_carries_fix(mailbox, monkeypatch, capsys):
    from fno.agents.registry import register_existing_session
    from fno.mail.cli import _name_lane_send

    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    register_existing_session(provider="codex", session_id=sid, cwd=str(mailbox))
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_codex", lambda *_a, **_k: False)
    _skip_mux(monkeypatch)
    monkeypatch.setattr("fno.mail.cli._codex_daemon_socket_absent", lambda: True)

    _name_lane_send("ping", from_name="sender", resolved=_resolved_codex(sid))

    out = capsys.readouterr().out
    assert "queued (durable)" in out
    assert "codex app-server daemon start" in out, "demote line names the fix command"


def test_mail_demote_reason_codex_daemon_present_no_hint(mailbox, monkeypatch, capsys):
    from fno.agents.registry import register_existing_session
    from fno.mail.cli import _name_lane_send

    sid = "9a063cd3-69d4-415a-ada5-649b0164189c"
    register_existing_session(provider="codex", session_id=sid, cwd=str(mailbox))
    monkeypatch.setattr("fno.agents.dispatch._mail_inject_codex", lambda *_a, **_k: False)
    _skip_mux(monkeypatch)
    monkeypatch.setattr("fno.mail.cli._codex_daemon_socket_absent", lambda: False)

    _name_lane_send("ping", from_name="sender", resolved=_resolved_codex(sid))

    out = capsys.readouterr().out
    assert "queued (durable)" in out
    assert "codex app-server daemon start" not in out


def test_store_healing_never_downgrades_an_operator_stamp(mailbox):
    """`adopted` is the weakest origin and must not overwrite a stronger one.

    `register_existing_session`'s refresh branch restamps `origin` whenever the
    caller passes one, and the store healer now passes `origin="adopted"`. So
    healing a hand-registered session by a token that missed the registry
    restamped `operator` as `adopted`, and `_recipient_is_attended` then read
    False forever - attended-miss escalation silently stopped firing for that
    session. The comment above that branch was written against exactly this
    clobber; the healer became the caller that delivers it.
    """
    from fno.agents.registry import register_existing_session, resolve_agent_in, load_registry

    sid = "3f2b71c0-11aa-4bb2-9cc3-5d6e7f809a1b"
    register_existing_session(
        provider="claude", session_id=sid, cwd=str(mailbox), origin="operator"
    )

    # The healer refreshing the same row, as it does on a store hit.
    register_existing_session(
        provider="claude", session_id=sid, cwd=str(mailbox), origin="adopted"
    )

    row = next(r for r in load_registry() if r.harness_session_id == sid)
    assert row.origin == "operator", "a store heal must not demote an operator session"


def test_store_healing_still_stamps_a_row_with_no_origin(mailbox):
    """The counterweight: the marker must still land where there is nothing to
    protect, or adopted rows stay indistinguishable from spawned ones."""
    from fno.agents.registry import register_existing_session, load_registry

    sid = "4a1c82d1-22bb-4cc3-8dd4-6e7f8a90b2c3"
    register_existing_session(provider="claude", session_id=sid, cwd=str(mailbox))
    register_existing_session(
        provider="claude", session_id=sid, cwd=str(mailbox), origin="adopted"
    )

    row = next(r for r in load_registry() if r.harness_session_id == sid)
    assert row.origin == "adopted"
