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
