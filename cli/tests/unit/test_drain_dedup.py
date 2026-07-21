"""US2: idempotent drain dedup by envelope id."""
from __future__ import annotations

import pytest

from fno.inbox.drain_dedup import already_seen, dedup_key, mark_seen


@pytest.fixture
def inbox_root(tmp_path, monkeypatch):
    # FNO_INBOX_ROOT lands the bus (and the drain-seen set) under tmp.
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    return tmp_path


@pytest.fixture
def repo_root(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("FNO_AUTO_MEMORY_DIR", str(tmp_path / "auto-memory"))
    return tmp_path


# ---- dedup_key (pure) ------------------------------------------------------

def test_dedup_key_present_for_id_bearing_envelope():
    body = '<fno_mail from="bob1234" harness="claude-code" model="opus" id="msg-abc">\nhi\n</fno_mail>'
    assert dedup_key(body) is not None


def test_dedup_key_none_when_no_id_attr():
    # A pre-redesign producer: the tag is present but carries no id -> un-dedupable.
    body = '<fno_mail from="bob1234" harness="claude-code" model="opus">\nhi\n</fno_mail>'
    assert dedup_key(body) is None
    assert dedup_key("plain body, no envelope") is None


def test_dedup_key_stable_for_identical_envelope_but_differs_by_content():
    # A byte-identical duplicate keys the same (true duplicate); an envelope with
    # the SAME id but different from/body keys differently, so a 24-bit id
    # collision between two distinct messages never causes a false drop.
    a1 = '<fno_mail from="aaaa1111" model="opus" id="msg-x">\nhello\n</fno_mail>'
    a2 = '<fno_mail from="aaaa1111" model="opus" id="msg-x">\nhello\n</fno_mail>'
    b = '<fno_mail from="bbbb2222" model="opus" id="msg-x">\ngoodbye\n</fno_mail>'
    assert dedup_key(a1) == dedup_key(a2)
    assert dedup_key(a1) != dedup_key(b)


# ---- seen-set roundtrip + bound --------------------------------------------

def test_mark_and_already_seen_roundtrip(inbox_root):
    assert not already_seen("alice", "msg-1")
    mark_seen("alice", "msg-1")
    assert already_seen("alice", "msg-1")
    # Per-recipient: bob has not seen alice's id.
    assert not already_seen("bob", "msg-1")


def test_seen_set_is_bounded(inbox_root):
    from fno.inbox import drain_dedup

    for i in range(drain_dedup._CAP + 50):
        mark_seen("alice", f"msg-{i}")
    # The oldest ids are evicted; the most recent survive.
    assert already_seen("alice", f"msg-{drain_dedup._CAP + 49}")
    assert not already_seen("alice", "msg-0")


# ---- drain integration -----------------------------------------------------

def _wrapped(msg_id: str) -> str:
    from fno.mail.envelope import wrap_fno_mail

    return wrap_fno_mail(
        "build done", from_="bob1234", harness="claude-code", model="opus", id=msg_id
    )


def test_dedup_drops_same_id_duplicate_exactly_once(inbox_root, repo_root):
    from fno.inbox.drain import drain_inbox
    from fno.inbox.store import Kind, write_new_thread

    body = _wrapped("msg-dup")
    # Two bounded-duplicate deliveries of ONE logical message: distinct durable
    # threads, same envelope id (Locked Decision 3).
    write_new_thread("alice", "bob1234", Kind.FYI.value, body, msg_id="msg-thread-a")
    write_new_thread("alice", "bob1234", Kind.FYI.value, body, msg_id="msg-thread-b")

    results = drain_inbox(repo_root, "alice")
    assert sorted(r.action for r in results) == ["deduped", "dismissed"]


def test_idless_messages_are_never_deduped(inbox_root, repo_root):
    from fno.inbox.drain import drain_inbox
    from fno.inbox.store import Kind, write_new_thread

    # No <fno_mail id> in either body -> both un-dedupable, both processed.
    write_new_thread("alice", "bob", Kind.FYI.value, "one plain body")
    write_new_thread("alice", "bob", Kind.FYI.value, "another plain body")

    results = drain_inbox(repo_root, "alice")
    assert [r.action for r in results] == ["dismissed", "dismissed"]
