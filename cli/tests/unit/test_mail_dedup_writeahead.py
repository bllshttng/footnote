"""W2: cross-delivery dedup at the drain + W3: write-ahead durable.

W2 (here, ``-k dedup``): a durable message whose ``msg_id`` already landed in the
recipient transcript (a live inject that confirmed after the durable copy was
written) is not printed again. The transcript is the ledger -- no new state --
read once per drain. A skipped message still advances the cursor and still emits
an ``agent_mail_drained`` receipt with ``reason="skipped-duplicate"``. A
transcript that cannot be read falls through to PRINTING: a read failure is not
evidence of absence, and a duplicate is strictly better than a drop
(AC4-HP, AC5-ERR).

W3 (``-k writeahead``) is appended alongside.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


class _Msg:
    def __init__(self, *, id, from_, to, kind, ts, body):  # noqa: A002
        self.id = id
        self.from_ = from_
        self.to = to
        self.kind = kind
        self.ts = ts
        self.body = body


class _CountingStdout:
    """Stands in for stdout: writes buffer until flush() moves them to delivered."""

    def __init__(self) -> None:
        self.buffer: list[str] = []
        self.delivered: list[str] = []

    def write(self, s: str) -> int:
        self.buffer.append(s)
        return len(s)

    def flush(self) -> None:
        self.delivered.extend(self.buffer)
        self.buffer.clear()

    def isatty(self) -> bool:
        return False


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


def _capture_events(monkeypatch) -> list[dict]:
    records: list[dict] = []
    from fno.agents import events as events_mod

    monkeypatch.setattr(
        events_mod, "emit", lambda kind, *, path=None, **data: records.append({"kind": kind, **data})
    )
    return records


# ---------------------------------------------------------------------------
# W2: cross-delivery dedup at the drain
# ---------------------------------------------------------------------------


def test_drain_skips_duplicate_already_in_transcript(monkeypatch):
    """AC4-HP: a durable copy whose id already landed in the transcript is not
    printed again, but the cursor still advances and the receipt carries
    reason=skipped-duplicate (silently swallowing it would recreate the absence
    problem one layer down)."""
    from fno.mail import cli as mail_cli

    msg = _Msg(
        id="m-dup", from_="alice", to="cl-abcd1234", kind="note",
        ts="2026-08-11T00:00:00Z", body="the duplicate body",
    )
    cursor_mod = _drain_setup(monkeypatch, msg)
    # The live inject already landed this id in the recipient transcript.
    monkeypatch.setattr("fno.mail.reply_resolve.present_mail_ids", lambda: {"m-dup"})
    advances: list[tuple] = []
    monkeypatch.setattr(
        cursor_mod, "advance_cursor", lambda h, mid: advances.append((h, mid)) or True
    )
    captured = _capture_events(monkeypatch)
    fake_out = _CountingStdout()

    with monkeypatch.context() as patch:
        patch.setattr("sys.stdout", fake_out)
        mail_cli.cmd_drain_self(json_out=False)

    assert "the duplicate body" not in "".join(fake_out.delivered), (
        "a duplicate was printed a second time"
    )
    assert advances, "a skipped duplicate did not advance the cursor"
    markers = [e for e in captured if e.get("kind") == "agent_mail_drained"]
    assert markers and markers[0]["msg_id"] == "m-dup", "no receipt for the skipped id"
    assert markers[0].get("reason") == "skipped-duplicate", (
        "a skipped duplicate was receipted as printed"
    )


def test_drain_prints_when_transcript_cannot_be_read(monkeypatch):
    """AC5-ERR: present_mail_ids() == None (unreadable transcript) must fall
    through to PRINTING. A read failure is not evidence of absence."""
    from fno.mail import cli as mail_cli

    msg = _Msg(
        id="m-ac5", from_="alice", to="cl-abcd1234", kind="note",
        ts="2026-08-11T00:00:00Z", body="must still print",
    )
    cursor_mod = _drain_setup(monkeypatch, msg)
    monkeypatch.setattr("fno.mail.reply_resolve.present_mail_ids", lambda: None)
    monkeypatch.setattr(cursor_mod, "advance_cursor", lambda h, mid: True)
    fake_out = _CountingStdout()

    with monkeypatch.context() as patch:
        patch.setattr("sys.stdout", fake_out)
        mail_cli.cmd_drain_self(json_out=False)

    assert "must still print" in "".join(fake_out.delivered), (
        "a message was skipped when the transcript could not be read"
    )


def test_drain_prints_message_absent_from_readable_transcript(monkeypatch):
    """Positive control for the partition: a readable transcript that does NOT
    carry the id prints normally (the dedup is a presence match, not an
    always-skip)."""
    from fno.mail import cli as mail_cli

    msg = _Msg(
        id="m-fresh", from_="alice", to="cl-abcd1234", kind="note",
        ts="2026-08-11T00:00:00Z", body="fresh message",
    )
    cursor_mod = _drain_setup(monkeypatch, msg)
    monkeypatch.setattr("fno.mail.reply_resolve.present_mail_ids", lambda: set())  # readable, empty
    monkeypatch.setattr(cursor_mod, "advance_cursor", lambda h, mid: True)
    captured = _capture_events(monkeypatch)
    fake_out = _CountingStdout()

    with monkeypatch.context() as patch:
        patch.setattr("sys.stdout", fake_out)
        mail_cli.cmd_drain_self(json_out=False)

    assert "fresh message" in "".join(fake_out.delivered)
    markers = [e for e in captured if e.get("kind") == "agent_mail_drained"]
    assert markers and markers[0].get("reason") == "printed"


def test_present_mail_ids_reads_envelope_ids_from_transcript(tmp_path, monkeypatch):
    """Positive control for the ledger instrument: a real claude transcript
    carrying <fno_mail id="..."> is read and its ids returned. Proves AC4's
    silence is the transcript match, not a no-op."""
    from fno.agents import discover
    from fno.harness_identity import resolve_harness_identity  # noqa: F401 -- import warm

    sid = "abcd1234"
    projects = tmp_path / "projects"
    proj = projects / "-Users-x-proj"
    proj.mkdir(parents=True, exist_ok=True)
    # A live-injected envelope lands verbatim in the jsonl (JSON-escaped quotes).
    record = {
        "type": "user",
        "message": {
            "content": (
                '<fno_mail from="alice" harness="claude" id="m-real">'
                " landed live </fno_mail>"
            )
        },
    }
    (proj / f"{sid}.jsonl").write_text(json.dumps(record) + "\n", encoding="utf-8")
    monkeypatch.setenv(discover.PROJECTS_DIR_ENV, str(projects))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", sid)
    monkeypatch.delenv("OPENCODE_SESSION_ID", raising=False)

    from fno.mail.reply_resolve import present_mail_ids

    ids = present_mail_ids()
    assert ids is not None, "a present transcript read as unreadable"
    assert "m-real" in ids


# ---------------------------------------------------------------------------
# W3: write the durable record ahead of the live attempt
# ---------------------------------------------------------------------------


def _register_claude_peer(name: str = "red") -> str:
    """One live claude peer with a full session id; returns its canonical handle
    (the durable recipient a send addresses)."""
    from fno.agents.registry import AgentEntry, write_registry
    from fno.harness_identity import canonical_handle

    sid = "abcd1234-1111-7222-8333-444455556666"
    write_registry([
        AgentEntry(
            name=name, harness="claude", harness_session_id=sid,
            cwd="/tmp", log_path="/tmp/red.log", short_id="abcd1234", status="live",
        )
    ])
    return canonical_handle(sid)


def test_writeahead_writes_durable_for_asleep_recipient(runner, tmp_path, monkeypatch):
    """AC8-HP: a send to a recipient we will not attempt live (asleep) writes the
    durable placeholder ahead. The recipient cannot drain during a live window
    (there is none), and a sender crash before it wakes leaves the message on the
    bus, drainable exactly once."""
    from fno.bus.cursor import scan_unread
    from fno.cli import app

    use_tmpdir(monkeypatch, tmp_path)
    recipient = _register_claude_peer("red")
    # Asleep: not live and not unknown -> not attemptable -> the write-ahead path.
    monkeypatch.setattr(
        "fno.agents.dispatch._registered_family1_state", lambda _e: "asleep"
    )

    res = runner.invoke(app, ["mail", "send", "red", "hi", "--from-name", "web"])
    assert res.exit_code == 0, f"exit={res.exit_code} out={res.output!r}"

    unread = scan_unread(recipient)
    assert unread, "the write-ahead placeholder did not land on the bus"
    assert all(m.kind != "withdraw" for m in unread)


def test_live_recipient_hosted_writes_no_durable(runner, tmp_path, monkeypatch):
    """A recipient we will attempt live does NOT write-ahead: it can drain during
    the live window, and a placeholder there would race the live inject. A hosted
    send to it leaves the bus empty."""
    from fno.bus.cursor import scan_unread
    from fno.cli import app

    use_tmpdir(monkeypatch, tmp_path)
    recipient = _register_claude_peer("red")
    monkeypatch.setattr(
        "fno.agents.dispatch._registered_family1_state", lambda _e: "working"
    )
    monkeypatch.setattr("fno.agents.dispatch._deliver_live", lambda *_a, **_k: True)

    res = runner.invoke(app, ["mail", "send", "red", "hi", "--from-name", "web"])
    assert res.exit_code == 0, f"exit={res.exit_code} out={res.output!r}"

    assert scan_unread(recipient) == [], (
        "a hosted live send wrote a durable copy (write-ahead must not fire for a "
        "recipient that can drain during the live window)"
    )


def test_live_recipient_live_miss_writes_durable(runner, tmp_path, monkeypatch):
    """An attemptable recipient whose live attempt misses falls back to the
    durable write (live-first), drainable exactly once."""
    from fno.bus.cursor import scan_unread
    from fno.cli import app

    use_tmpdir(monkeypatch, tmp_path)
    recipient = _register_claude_peer("red")
    monkeypatch.setattr(
        "fno.agents.dispatch._registered_family1_state", lambda _e: "working"
    )
    monkeypatch.setattr("fno.agents.dispatch._deliver_live", lambda *_a, **_k: False)

    res = runner.invoke(app, ["mail", "send", "red", "hi", "--from-name", "web"])
    assert res.exit_code == 0, f"exit={res.exit_code} out={res.output!r}"

    unread = scan_unread(recipient)
    assert unread, "the live-first durable fallback did not write"
    assert all(m.kind != "withdraw" for m in unread)

