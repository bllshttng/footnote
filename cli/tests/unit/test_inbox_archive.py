"""Tests for the post-2026-05 inbox archive (thread-file rotation)."""
from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path

import pytest


@pytest.fixture
def inbox_root(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    return tmp_path


def test_unread_threads_are_never_archived(inbox_root):
    from fno.inbox.archive import InboxSettings, archive_old_threads
    from fno.inbox.store import write_new_thread, Kind

    write_new_thread("alice", "bob", Kind.FYI.value, "unread one")
    write_new_thread("alice", "bob", Kind.HEADS_UP.value, "unread two")

    result = archive_old_threads("alice", InboxSettings(keep_recent_read=0))
    assert result.archived_count == 0
    assert result.kept_unread == 2


def test_archive_moves_oldest_read_threads_only(inbox_root):
    from fno.inbox.archive import InboxSettings, archive_old_threads
    from fno.inbox.store import write_new_thread, mark_thread_read, Kind, inbox_dir_for

    handles = []
    for i in range(5):
        h = write_new_thread("alice", "bob", Kind.FYI.value, f"msg {i} content")
        handles.append(h)

    # Mark 3 as read at distinct times (simulate older read first)
    base = datetime(2026, 1, 1, tzinfo=timezone.utc)
    for i, h in enumerate(handles[:3]):
        ts = base.replace(day=1 + i)
        mark_thread_read(h.path, ts=ts)

    result = archive_old_threads("alice", InboxSettings(keep_recent_read=1))
    assert result.archived_count == 2
    assert result.kept_unread == 2
    assert result.kept_recent_read == 1

    # Live inbox now has 2 unread + 1 read = 3 files
    inbox = inbox_dir_for("alice")
    live_files = sorted(p for p in inbox.glob("*.md"))
    assert len(live_files) == 3

    # Archived files land in inbox/archive/{YYYY-MM}/
    archive_dir = inbox / "archive"
    assert archive_dir.exists()
    archived_files = list(archive_dir.rglob("*.md"))
    assert len(archived_files) == 2


def test_archive_idempotent_on_rerun(inbox_root):
    from fno.inbox.archive import InboxSettings, archive_old_threads
    from fno.inbox.store import write_new_thread, mark_thread_read, Kind

    h = write_new_thread("alice", "bob", Kind.FYI.value, "content")
    mark_thread_read(h.path, ts=datetime(2026, 1, 1, tzinfo=timezone.utc))

    first = archive_old_threads("alice", InboxSettings(keep_recent_read=0))
    second = archive_old_threads("alice", InboxSettings(keep_recent_read=0))
    assert first.archived_count == 1
    assert second.archived_count == 0


def test_archive_no_inbox_dir_returns_zero(inbox_root):
    from fno.inbox.archive import InboxSettings, archive_old_threads

    result = archive_old_threads("ghost-recipient", InboxSettings())
    assert result.archived_count == 0


def test_legacy_rotate_raises_helpful_error(inbox_root):
    from fno.inbox.archive import rotate

    with pytest.raises(NotImplementedError, match="archive_old_threads"):
        rotate(Path("/tmp/whatever"), None)
