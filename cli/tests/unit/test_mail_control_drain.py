"""Sender-side stamping for the control lane (tool-boundary delivery).

A control body that demoted durable must reach the recipient at its next
TOOL boundary, not wait for the prompt boundary a busy worker never
reaches. The durable write stamps a per-recipient pending flag the
PreToolUse hook gates on; the drain itself is the fno-agents
``mail-control-drain`` verb (see crates/fno-agents/src/mail_control_drain.rs).
"""
from __future__ import annotations

import pytest

from fno.paths_testing import use_tmpdir

MARKERS = ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID")
MY_SID = "ffffabcd1234"  # canonical_handle -> ffffabcd (first-eight)
MY_HANDLE = "ffffabcd"


@pytest.fixture
def env(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    for m in MARKERS:
        monkeypatch.delenv(m, raising=False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", MY_SID)
    return tmp_path


def _flag_path(form):
    from fno import paths

    return paths.bus_dir() / "control-pending" / f"{form}.flag"


def _seed_thread(recipient):
    from fno.inbox.store import inbox_dir_for, write_new_thread

    write_new_thread(recipient, sender="alice", kind="send", body="thread seed")
    inbox = inbox_dir_for(recipient)
    threads = sorted(inbox.glob("*.md"))
    assert threads, "seed thread missing"
    return threads[-1]


def test_the_control_lane_stamps_pending_flags(env):
    from fno.inbox.store import write_new_thread

    _ordinary_write_leaves_no_flag(env)

    handle = write_new_thread(
        MY_HANDLE,
        sender="lead",
        kind="send",
        body="control: freeze - hold",
    )
    assert handle is not None
    assert _flag_path(MY_HANDLE).exists()
    _control_reply_append_marks_pending(env)


def _ordinary_write_leaves_no_flag(env):
    from fno.inbox.store import write_new_thread

    write_new_thread(MY_HANDLE, sender="alice", kind="send", body="plain status")
    assert not _flag_path(MY_HANDLE).exists()


def _control_reply_append_marks_pending(env):
    from fno.inbox.store import append_to_thread

    thread = _seed_thread(MY_HANDLE)
    append_to_thread(thread, sender="lead", body="control: hold")
    assert _flag_path(MY_HANDLE).exists()
