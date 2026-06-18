"""Tests for scripts/migrate-inbox-flat-to-threads.py round-tripping."""
from __future__ import annotations

import importlib.util
import sys
from datetime import datetime, timezone
from pathlib import Path

import pytest


def _load_migration_module():
    repo_root = Path(__file__).resolve().parents[3]
    script_path = repo_root / "scripts" / "migrate-inbox-flat-to-threads.py"
    spec = importlib.util.spec_from_file_location("inbox_migration", script_path)
    mod = importlib.util.module_from_spec(spec)
    sys.modules["inbox_migration"] = mod
    spec.loader.exec_module(mod)
    return mod


@pytest.fixture
def inbox_root(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    return tmp_path


_FLAT_FIXTURE = """\
---
created: 2026-05-07T11:18
updated: 2026-05-07T11:37
---
# Inbox: alice

This file holds messages addressed to the alice agent.
Edit only via `fno inbox` commands.

## msg-c74627 · 2026-05-07 18:18 · from:bob · kind:heads-up
status: read
ref_pr: 409
triaged_into: ab-feedface

Backend now exposes region column on /api/records; web should plumb it.

## msg-d11111 · 2026-05-07 18:25 · from:alice · kind:notification · reply_to:msg-c74627
status: read
triaged_into: null

Got it; will plumb today.

## msg-e22222 · 2026-05-07 19:00 · from:carol · kind:question
status: unread
triaged_into: null

Should we roll the rotation queue back to 5 swaps before 3am?

## msg-f33333 · 2026-05-07 20:00 · from:dave · kind:lesson
status: read
triaged_into: null

filelock 3.x and fcntl.flock are wire-compatible on macOS; reuse the lock path.
"""


def _seed_flat(inbox_root: Path, project: str, content: str) -> Path:
    proj = inbox_root / project
    proj.mkdir(parents=True, exist_ok=True)
    flat = proj / "inbox.md"
    flat.write_text(content, encoding="utf-8")
    return flat


def test_migration_round_trips_kinds_and_threads(inbox_root):
    migration = _load_migration_module()
    flat = _seed_flat(inbox_root, "alice", _FLAT_FIXTURE)

    result = migration.migrate_project("alice", flat, dry_run=False)
    assert result["errors"] == []
    assert result["threads_written"] == 3  # heads-up + question + lesson roots
    assert result["messages_migrated"] == 4

    # inbox.md was moved to safety-net path
    assert not flat.exists()
    assert (inbox_root / "alice" / "inbox-pre-migration.md").exists()

    # Three thread files in inbox/
    thread_files = sorted((inbox_root / "alice" / "inbox").glob("*.md"))
    assert len(thread_files) == 3


def test_migration_collapses_reply_chain_into_one_thread(inbox_root):
    """msg-c74627 + msg-d11111 (reply_to:msg-c74627) -> single thread file."""
    migration = _load_migration_module()
    flat = _seed_flat(inbox_root, "alice", _FLAT_FIXTURE)

    migration.migrate_project("alice", flat, dry_run=False)

    from fno.inbox.store import find_thread_by_msg_id

    parent = find_thread_by_msg_id("alice", "msg-c74627")
    reply = find_thread_by_msg_id("alice", "msg-d11111")
    assert parent is not None and reply is not None
    assert parent.path == reply.path
    assert len(parent.messages) == 2


def test_migration_status_read_becomes_read_at(inbox_root):
    migration = _load_migration_module()
    flat = _seed_flat(inbox_root, "alice", _FLAT_FIXTURE)

    migration.migrate_project("alice", flat, dry_run=False)

    from fno.inbox.store import find_thread_by_msg_id

    # heads-up thread: both messages were read, so the thread is read.
    heads_up = find_thread_by_msg_id("alice", "msg-c74627")
    assert heads_up is not None and heads_up.read_at is not None

    # question thread: unread -> stays unread.
    question = find_thread_by_msg_id("alice", "msg-e22222")
    assert question is not None and question.read_at is None


def test_migration_lesson_kind_collapses_to_fyi_with_persist(inbox_root):
    migration = _load_migration_module()
    flat = _seed_flat(inbox_root, "alice", _FLAT_FIXTURE)

    migration.migrate_project("alice", flat, dry_run=False)

    from fno.inbox.store import find_thread_by_msg_id

    lesson = find_thread_by_msg_id("alice", "msg-f33333")
    assert lesson is not None
    assert lesson.kind == "fyi"
    assert lesson.persist_to_memory is True


def test_migration_idempotent_when_inbox_pre_migration_exists(inbox_root):
    migration = _load_migration_module()
    flat = _seed_flat(inbox_root, "alice", _FLAT_FIXTURE)

    first = migration.migrate_project("alice", flat, dry_run=False)
    assert first["errors"] == []

    # Second run: source file is gone (renamed), the safety-net path exists.
    flat_path = inbox_root / "alice" / "inbox.md"
    second = migration.migrate_project("alice", flat_path, dry_run=False)
    assert second["skipped"] is True
    assert "already migrated" in second["reason"]


def test_migration_dry_run_writes_nothing(inbox_root):
    migration = _load_migration_module()
    flat = _seed_flat(inbox_root, "alice", _FLAT_FIXTURE)

    result = migration.migrate_project("alice", flat, dry_run=True)
    assert result["threads_written"] == 3

    # No files written, original inbox.md untouched.
    assert flat.exists()
    inbox_dir = inbox_root / "alice" / "inbox"
    assert not inbox_dir.exists() or not list(inbox_dir.glob("*.md"))


def test_migration_handles_empty_inbox(inbox_root):
    migration = _load_migration_module()
    flat = _seed_flat(inbox_root, "alice", "# Inbox: alice\n\nempty.\n")

    result = migration.migrate_project("alice", flat, dry_run=False)
    assert result["skipped"] is True
    assert "no parseable" in result["reason"]
    # File still present (we did not move it)
    assert flat.exists()
