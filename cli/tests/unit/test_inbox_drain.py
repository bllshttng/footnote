"""Tests for the post-2026-05 inbox drain dispatcher (3 handlers)."""
from __future__ import annotations

import json
import os
from datetime import datetime, timezone
from pathlib import Path

import pytest


@pytest.fixture
def inbox_root(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    return tmp_path


@pytest.fixture
def repo_root(tmp_path, monkeypatch):
    """Pin the .fno/ output tree to a tmp dir.

    Also redirects the inbox-drain auto-memory writer (otherwise it would
    write under ~/.claude/projects/ based on tmp_path's encoded form,
    leaving orphan dirs across the user's home after every test run).
    """
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("FNO_AUTO_MEMORY_DIR", str(tmp_path / "auto-memory"))
    return tmp_path


def test_question_drops_wake_signal_leaves_unread(inbox_root, repo_root):
    from fno.inbox.drain import drain_inbox
    from fno.inbox.store import write_new_thread, Kind, read_unread_threads

    handle = write_new_thread(
        "alice", "bob", Kind.QUESTION.value, "should we roll back the queue?",
    )

    results = drain_inbox(repo_root, "alice")
    assert len(results) == 1
    assert results[0].action == "wake_signal_dropped"
    assert results[0].kind == "question"

    # Wake signal landed under .fno/wake-signals
    sigs = list((repo_root / ".fno" / "wake-signals").glob("wake-*.json"))
    assert len(sigs) == 1

    # Thread stays unread
    assert read_unread_threads("alice")[0].path == handle.path


def test_fyi_default_logs_to_convo_signals(inbox_root, repo_root):
    from fno.inbox.drain import drain_inbox
    from fno.inbox.store import write_new_thread, Kind, read_unread_threads

    write_new_thread("alice", "bob", Kind.FYI.value, "build complete")
    results = drain_inbox(repo_root, "alice")
    assert len(results) == 1
    assert results[0].action == "logged"
    assert results[0].kind == "fyi"
    log = (repo_root / ".fno" / "convo-signals.jsonl").read_text()
    entries = [json.loads(line) for line in log.strip().splitlines()]
    assert any(e["event"] == "inbox_fyi" for e in entries)
    # Thread is now read
    assert read_unread_threads("alice") == []


def test_fyi_persist_memory_writes_memory_file(inbox_root, repo_root, tmp_path):
    from fno.inbox.drain import drain_inbox
    from fno.inbox.store import write_new_thread, Kind

    write_new_thread(
        "alice", "bob", Kind.FYI.value, "useful lesson body",
        persist_to_memory=True,
    )
    results = drain_inbox(repo_root, "alice")
    assert len(results) == 1
    assert results[0].action == "memory_written"
    memory_path = Path(results[0].memory_path)
    assert memory_path.exists()
    text = memory_path.read_text()
    assert "source_inbox_thread:" in text
    assert "useful lesson body" in text


def test_drain_caps_at_max_threads(inbox_root, repo_root, monkeypatch):
    from fno.inbox.drain import drain_inbox
    from fno.inbox.store import write_new_thread, Kind

    for i in range(15):
        write_new_thread(
            "alice", "bob", Kind.FYI.value, f"msg {i} body content",
        )

    results = drain_inbox(repo_root, "alice", max_threads=5)
    assert len(results) == 5


def test_unknown_kind_in_thread_file_returns_unknown_kind(inbox_root, repo_root):
    """A thread file with a malformed/legacy kind should not crash drain."""
    from fno.inbox.drain import drain_inbox
    from fno.inbox.store import inbox_dir_for

    inbox = inbox_dir_for("alice")
    inbox.mkdir(parents=True)
    (inbox / "2026-05-08-bogus.md").write_text(
        "---\n"
        "thread_id: msg-aaa111\n"
        "from: bob\n"
        "to: alice\n"
        "kind: legacy-kind-value\n"
        "created: 2026-05-08T10:00:00Z\n"
        "---\n\n"
        "## msg-aaa111 · 2026-05-08T10:00:00Z · from:bob\n\n"
        "body text\n",
        encoding="utf-8",
    )

    results = drain_inbox(repo_root, "alice")
    assert len(results) == 1
    assert results[0].action == "unknown_kind"
    assert results[0].error is not None


def test_heads_up_create_node_marks_read(inbox_root, repo_root, monkeypatch):
    """Mock triage_thread to return create_node, mock subprocess for `fno new`."""
    import subprocess as _subprocess

    from fno.inbox import drain as drain_mod
    from fno.inbox.store import (
        write_new_thread, Kind, read_unread_threads,
    )
    from fno.inbox.triage import TriagePlan

    handle = write_new_thread(
        "alice", "bob", Kind.HEADS_UP.value, "add region column",
    )

    def fake_triage_thread(h, **_kwargs):
        return TriagePlan(
            action="create_node",
            title="add region column",
            priority="p2",
            body="proposed work item",
            follow_up_question=None,
        )

    class _FakeRun:
        def __init__(self, returncode=0, stdout="", stderr=""):
            self.returncode = returncode
            self.stdout = stdout
            self.stderr = stderr

    def fake_run(cmd, *args, **kwargs):
        if cmd[:3] == ["fno-py", "new", "--help"]:
            return _FakeRun(stdout="--source-inbox-thread\n")
        if cmd[:2] == ["fno-py", "new"]:
            return _FakeRun(stdout="created node ab-abc123\n")
        raise AssertionError(f"unexpected subprocess: {cmd}")

    monkeypatch.setattr(drain_mod, "triage_thread", fake_triage_thread)
    monkeypatch.setattr(drain_mod.subprocess, "run", fake_run)

    results = drain_mod.drain_inbox(repo_root, "alice")
    assert len(results) == 1
    assert results[0].action == "created_node"
    assert results[0].node_id == "ab-abc123"

    assert read_unread_threads("alice") == []


def test_heads_up_request_clarification_leaves_unread(inbox_root, repo_root, monkeypatch):
    from fno.inbox import drain as drain_mod
    from fno.inbox.store import write_new_thread, Kind, read_unread_threads
    from fno.inbox.triage import TriagePlan

    write_new_thread("alice", "bob", Kind.HEADS_UP.value, "ambiguous request")

    monkeypatch.setattr(
        drain_mod, "triage_thread",
        lambda h, **kw: TriagePlan(
            action="request_clarification",
            title=None,
            priority=None,
            body="need more info",
            follow_up_question="what migration window?",
        ),
    )

    results = drain_mod.drain_inbox(repo_root, "alice")
    assert results[0].action == "clarification_pending"
    assert len(read_unread_threads("alice")) == 1


def test_heads_up_ignore_marks_read(inbox_root, repo_root, monkeypatch):
    from fno.inbox import drain as drain_mod
    from fno.inbox.store import write_new_thread, Kind, read_unread_threads
    from fno.inbox.triage import TriagePlan

    write_new_thread("alice", "bob", Kind.HEADS_UP.value, "noise")

    monkeypatch.setattr(
        drain_mod, "triage_thread",
        lambda h, **kw: TriagePlan(
            action="ignore",
            title=None,
            priority=None,
            body="not actionable",
            follow_up_question=None,
        ),
    )

    results = drain_mod.drain_inbox(repo_root, "alice")
    assert results[0].action == "ignored"
    assert read_unread_threads("alice") == []


def test_resolve_memory_dir_keys_to_canonical_worktree(tmp_path, monkeypatch):
    """The memory dir keys to the canonical working tree resolved by the shared
    helper (which skips bare / separate-git-dir; covered in
    test_resolve_canonical_worktree.py). Here: drain uses the helper's result,
    not the linked worktree it was called from.
    """
    monkeypatch.delenv("FNO_AUTO_MEMORY_DIR", raising=False)

    canonical = tmp_path / "wt-main"
    canonical.mkdir()
    linked = tmp_path / "wt-linked"
    linked.mkdir()

    from fno.inbox import drain as drain_mod
    monkeypatch.setattr(drain_mod, "resolve_canonical_worktree", lambda *a, **k: canonical)

    memory_dir = drain_mod._resolve_memory_dir(linked)
    encoded = canonical.resolve().as_posix().replace(":", "").replace("/", "-")
    assert memory_dir == Path.home() / ".claude" / "projects" / encoded / "memory"


def test_resolve_memory_dir_falls_back_when_no_worktree(tmp_path, monkeypatch):
    """When the helper finds no usable working tree (non-git / bare-only), key
    to repo_root as before."""
    monkeypatch.delenv("FNO_AUTO_MEMORY_DIR", raising=False)

    repo = tmp_path / "repo"
    repo.mkdir()

    from fno.inbox import drain as drain_mod
    monkeypatch.setattr(drain_mod, "resolve_canonical_worktree", lambda *a, **k: None)

    memory_dir = drain_mod._resolve_memory_dir(repo)
    encoded = repo.resolve().as_posix().replace(":", "").replace("/", "-")
    assert memory_dir == Path.home() / ".claude" / "projects" / encoded / "memory"


def test_send_kind_drains_with_fyi_semantics(inbox_root, repo_root):
    """codex #459 P1: kind=send threads (from `fno mail send`) must drain
    (fyi semantics: surface + mark read), not loop forever as unknown_kind."""
    from fno.inbox.drain import drain_inbox
    from fno.inbox.store import write_new_thread, Kind, read_unread_threads

    write_new_thread("alice", "bus-peer", Kind.SEND.value, "fyi built the thing")
    results = drain_inbox(repo_root, "alice")
    assert len(results) == 1
    assert results[0].action != "unknown_kind"
    assert results[0].action == "logged"
    # Thread is consumed (marked read), not stranded unread.
    assert read_unread_threads("alice") == []
