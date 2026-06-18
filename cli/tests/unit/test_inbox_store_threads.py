"""Unit tests for the thread-per-file inbox store (post-2026-05 layout)."""
from __future__ import annotations

import os
from datetime import datetime, timezone
from pathlib import Path

import pytest


@pytest.fixture
def inbox_root(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    return tmp_path


def test_write_new_thread_creates_file_under_inbox_dir(inbox_root):
    from fno.inbox.store import write_new_thread, Kind, inbox_dir_for

    h = write_new_thread(
        "alice", "bob", Kind.HEADS_UP.value, "first line slug seed text here"
    )
    assert h.path.exists()
    assert h.path.parent == inbox_dir_for("alice")
    assert h.path.suffix == ".md"
    assert h.is_unread
    assert h.from_project == "bob"
    assert h.to_project == "alice"
    assert h.kind == Kind.HEADS_UP.value


def test_filename_uses_first_5_words_kebab_lowercase(inbox_root):
    from fno.inbox.store import write_new_thread, Kind

    h = write_new_thread(
        "alice", "bob", Kind.HEADS_UP.value,
        "Add region column to records table please",
    )
    assert h.path.name.endswith(".md")
    assert "add-region-column-to-records" in h.path.name


def test_filename_caps_at_max_len_and_has_iso_date(inbox_root):
    from fno.inbox.store import write_new_thread, Kind

    long_body = " ".join(["supercalifragilistic"] * 5)  # 100+ chars naively
    ts = datetime(2026, 5, 7, 11, 18, tzinfo=timezone.utc)
    h = write_new_thread(
        "alice", "bob", Kind.FYI.value, long_body, timestamp=ts,
    )
    assert h.path.name.startswith("2026-05-07-")
    stem_after_date = h.path.stem[len("2026-05-07-"):]
    assert len(stem_after_date) <= 40


def test_filename_collisions_get_numeric_suffix(inbox_root):
    from fno.inbox.store import write_new_thread, Kind

    body = "same first words different content"
    h1 = write_new_thread("alice", "bob", Kind.FYI.value, body)
    h2 = write_new_thread("alice", "bob", Kind.FYI.value, body)
    assert h1.path != h2.path
    assert h1.path.parent == h2.path.parent


def test_append_to_thread_keeps_one_file(inbox_root):
    from fno.inbox.store import (
        write_new_thread, append_to_thread, read_thread, Kind,
    )

    h = write_new_thread("alice", "bob", Kind.HEADS_UP.value, "root message body")
    new_id = append_to_thread(h.path, "alice", "reply body")
    assert new_id != h.thread_id

    re_read = read_thread(h.path)
    assert re_read is not None
    assert len(re_read.messages) == 2
    assert re_read.messages[0].from_project == "bob"
    assert re_read.messages[1].from_project == "alice"
    assert re_read.is_unread


def test_find_thread_by_msg_id_root(inbox_root):
    from fno.inbox.store import write_new_thread, find_thread_by_msg_id, Kind

    h = write_new_thread("alice", "bob", Kind.QUESTION.value, "hello")
    found = find_thread_by_msg_id("alice", h.thread_id)
    assert found is not None and found.path == h.path


def test_find_thread_by_msg_id_appended_reply(inbox_root):
    from fno.inbox.store import (
        write_new_thread, append_to_thread, find_thread_by_msg_id, Kind,
    )

    h = write_new_thread("alice", "bob", Kind.HEADS_UP.value, "root msg body")
    reply_id = append_to_thread(h.path, "alice", "reply text")
    found = find_thread_by_msg_id("alice", reply_id)
    assert found is not None and found.path == h.path


def test_find_thread_by_msg_id_returns_none_when_missing(inbox_root):
    from fno.inbox.store import find_thread_by_msg_id

    assert find_thread_by_msg_id("alice", "msg-deadbeef") is None


def test_read_unread_threads_filters_by_read_at(inbox_root):
    from fno.inbox.store import (
        write_new_thread, mark_thread_read, read_unread_threads, Kind,
    )

    a = write_new_thread("alice", "bob", Kind.HEADS_UP.value, "first one")
    b = write_new_thread("alice", "bob", Kind.FYI.value, "second one")
    c = write_new_thread("alice", "bob", Kind.QUESTION.value, "third one")
    mark_thread_read(b.path)

    unread = read_unread_threads("alice")
    paths = {h.path for h in unread}
    assert a.path in paths
    assert c.path in paths
    assert b.path not in paths


def test_mark_thread_read_writes_iso_frontmatter(inbox_root):
    from fno.inbox.store import write_new_thread, mark_thread_read, read_thread, Kind

    h = write_new_thread("alice", "bob", Kind.FYI.value, "content")
    mark_thread_read(h.path)
    re_read = read_thread(h.path)
    assert re_read is not None and re_read.read_at is not None
    text = h.path.read_text(encoding="utf-8")
    assert "read_at:" in text


def test_persist_to_memory_round_trips(inbox_root):
    from fno.inbox.store import write_new_thread, read_thread, Kind

    h = write_new_thread(
        "alice", "bob", Kind.FYI.value, "lesson body here",
        persist_to_memory=True,
    )
    re_read = read_thread(h.path)
    assert re_read is not None and re_read.persist_to_memory is True


def test_replies_to_round_trips(inbox_root):
    from fno.inbox.store import write_new_thread, read_thread, Kind

    h = write_new_thread(
        "alice", "bob", Kind.FYI.value, "cross thread ref",
        replies_to="msg-deadbeef",
    )
    re_read = read_thread(h.path)
    assert re_read is not None and re_read.replies_to == "msg-deadbeef"


def test_refs_round_trip(inbox_root):
    from fno.inbox.store import write_new_thread, read_thread, Kind

    h = write_new_thread(
        "alice", "bob", Kind.HEADS_UP.value, "reffy body",
        refs={"ref_pr": "112", "ref_node": "ab-feedface"},
    )
    re_read = read_thread(h.path)
    assert re_read is not None
    assert re_read.refs.get("ref_pr") == "112"
    assert re_read.refs.get("ref_node") == "ab-feedface"


def test_inbox_dir_rejects_path_separator(inbox_root):
    from fno.inbox.store import inbox_dir_for

    with pytest.raises(ValueError):
        inbox_dir_for("../../etc/passwd")


@pytest.mark.parametrize(
    "bad",
    [
        "..",
        "../etc",
        ".hidden",
        "name with space",
        "name/sub",
        "name\\sub",
        "",
    ],
)
def test_inbox_dir_rejects_traversal_and_invalid_names(inbox_root, bad):
    from fno.inbox.store import inbox_dir_for

    with pytest.raises(ValueError):
        inbox_dir_for(bad)


def test_append_to_thread_resets_read_at(inbox_root):
    """A reply to a drained thread must resurface it for the next drain."""
    from fno.inbox.store import (
        write_new_thread, append_to_thread, mark_thread_read,
        read_thread, read_unread_threads, Kind,
    )

    h = write_new_thread("alice", "bob", Kind.HEADS_UP.value, "first message body")
    mark_thread_read(h.path)
    assert read_unread_threads("alice") == []

    append_to_thread(h.path, "carol", "follow-up reply body")

    re_read = read_thread(h.path)
    assert re_read is not None and re_read.is_unread
    unread = read_unread_threads("alice")
    assert len(unread) == 1 and unread[0].path == h.path


def test_concurrent_writers_do_not_clobber_filenames(inbox_root):
    """Two senders with the same body/date must NOT overwrite each other.

    The atomic O_CREAT|O_EXCL claim plus suffix bump guarantees both
    files exist on disk after a tight loop.
    """
    from fno.inbox.store import write_new_thread, Kind, inbox_dir_for

    body = "race to the same slug today"
    handles = [
        write_new_thread("alice", "bob", Kind.FYI.value, body)
        for _ in range(10)
    ]
    paths = {h.path for h in handles}
    assert len(paths) == 10
    files = sorted(inbox_dir_for("alice").glob("*.md"))
    assert len(files) == 10


def test_atomic_write_does_not_truncate_on_failure(inbox_root, monkeypatch):
    """A render-write failure mid-append leaves the prior render intact AND does
    not fail the send - the reply is durable on the bus (ab-cee91152 Move A).

    Post-flip contract: the markdown render is derived/best-effort. A render
    write failure is logged, never raised; the durable bus append already landed,
    so the reply survives and ``rebuild_render`` can regenerate the render.
    """
    from fno.inbox import store
    from fno.inbox.store import (
        write_new_thread, append_to_thread, read_thread, Kind,
    )
    from fno.bus.log import iter_thread

    h = write_new_thread("alice", "bob", Kind.FYI.value, "original body content")
    original_text = h.path.read_text()

    real_replace = store.os.replace

    def boom(*_args, **_kwargs):
        raise OSError("disk full")

    monkeypatch.setattr(store.os, "replace", boom)
    # The render update fails, but the append SUCCEEDS (best-effort render).
    new_id = append_to_thread(h.path, "carol", "doomed reply")
    assert new_id

    monkeypatch.setattr(store.os, "replace", real_replace)
    # The prior render file is byte-for-byte intact (atomic write never truncates).
    assert h.path.read_text() == original_text
    re_read = read_thread(h.path)
    assert re_read is not None and len(re_read.messages) == 1
    # The reply is durable on the canonical bus log despite the render failure.
    convo = list(iter_thread(h.thread_id))
    assert [m.id for m in convo] == [h.thread_id, new_id]


def test_kind_enum_exact_membership():
    from fno.inbox.store import Kind, VALID_KINDS

    # "send" added by the cross-agent message bus Group 2 (ab-3bab5741):
    # agent-to-agent envelopes written by `fno mail send` (US3).
    assert {k.value for k in Kind} == {"heads-up", "question", "fyi", "send"}
    assert VALID_KINDS == frozenset({"heads-up", "question", "fyi", "send"})


def test_deprecated_kinds_map_to_replacements():
    from fno.inbox.store import DEPRECATED_KINDS

    assert "notification" in DEPRECATED_KINDS
    assert "lesson" in DEPRECATED_KINDS
    assert "answer" in DEPRECATED_KINDS
    assert "fyi" in DEPRECATED_KINDS["notification"]
    assert "memory" in DEPRECATED_KINDS["lesson"]
    assert "reply-to" in DEPRECATED_KINDS["answer"]


def test_resolve_project_walks_settings_yaml(tmp_path, monkeypatch):
    from fno.inbox.store import resolve_project, ProjectIdentificationError

    project_dir = tmp_path / "myproj"
    settings_dir = project_dir / ".fno"
    settings_dir.mkdir(parents=True)
    (settings_dir / "settings.yaml").write_text("project: myproj\n", encoding="utf-8")

    sub = project_dir / "deep" / "tree"
    sub.mkdir(parents=True)
    assert resolve_project(cwd=sub) == "myproj"
    assert resolve_project(override="other") == "other"


def test_resolve_project_raises_when_unset(tmp_path, monkeypatch):
    from fno.inbox.store import resolve_project, ProjectIdentificationError

    monkeypatch.chdir(tmp_path)
    with pytest.raises(ProjectIdentificationError):
        resolve_project(cwd=tmp_path)


def test_concurrent_appends_serialize(inbox_root):
    """Two appenders must not interleave. Sequential calls roundtrip cleanly."""
    from fno.inbox.store import write_new_thread, append_to_thread, read_thread, Kind

    h = write_new_thread("alice", "bob", Kind.HEADS_UP.value, "root body")
    ids = [append_to_thread(h.path, f"sender-{i}", f"reply {i}") for i in range(5)]

    re_read = read_thread(h.path)
    assert re_read is not None
    msg_ids = [m.msg_id for m in re_read.messages]
    assert msg_ids[0] == h.thread_id
    for i, mid in enumerate(ids):
        assert mid in msg_ids
