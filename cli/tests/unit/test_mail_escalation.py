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


def test_fyi_and_heads_up_do_not_escalate(runner, mailbox, notified):
    for kind in ("fyi", "heads-up"):
        runner.invoke(
            app,
            ["mail", "send", "--to-project", "web", "--kind", kind,
             "--from-name", "etl", "--body", f"a {kind}"],
        )
    assert notified == [], "only question escalates to the human"


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
