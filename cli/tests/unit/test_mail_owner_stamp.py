"""US6: terminal ownership classification rides the bus envelope meta.

The bus JSONL is the system of record the dead-letter sweep reads (LD11), so a
durable write's ``owner``/``ttl_at`` must land in the envelope ``meta``, and a
caller that omits them must write a thread byte-identical to before.
"""
from __future__ import annotations

from datetime import datetime, timezone

from fno.inbox.store import (
    DurableOwner,
    classify_durable_owner,
    post_inbox_message,
    ttl_at_for,
    write_new_thread,
)
from fno.paths_testing import use_tmpdir


def _bus():
    from fno.bus.log import iter_messages

    return list(iter_messages())


def test_classify_matrix():
    c = classify_durable_owner
    assert c(param_forced=True, recipient_live=True, recipient_resumable=True) is DurableOwner.INBOX_DRAIN
    assert c(param_forced=False, recipient_live=True, recipient_resumable=False) is DurableOwner.LIVE_DRAIN
    assert c(param_forced=False, recipient_live=False, recipient_resumable=True) is DurableOwner.WAKE_DAEMON
    assert c(param_forced=False, recipient_live=False, recipient_resumable=False) is DurableOwner.DEAD_LETTER


def test_dead_letter_ttl_is_birth():
    now = datetime(2026, 7, 22, tzinfo=timezone.utc)
    assert ttl_at_for(DurableOwner.DEAD_LETTER, now) == now
    assert ttl_at_for(DurableOwner.WAKE_DAEMON, now) > now


def test_write_new_thread_stamps_owner_and_ttl_on_bus(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    write_new_thread("claude-cafe0001", "web", "send", "hello", owner=DurableOwner.WAKE_DAEMON.value)
    msgs = _bus()
    assert len(msgs) == 1
    assert msgs[0].meta.get("owner") == "wake-daemon"
    assert msgs[0].meta.get("ttl_at")  # derived from owner class + created ts


def test_omitted_owner_writes_no_owner_meta(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    write_new_thread("claude-cafe0001", "web", "send", "hi")
    msgs = _bus()
    assert "owner" not in msgs[0].meta
    assert "ttl_at" not in msgs[0].meta


def test_post_inbox_message_owns_as_inbox_drain(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    post_inbox_message(recipient="footnote", sender="web", kind="heads-up", body="note")
    msgs = _bus()
    assert msgs[0].meta.get("owner") == "inbox-drain"
