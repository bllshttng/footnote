"""Control-mail landing at the tool boundary (``fno agents mail control-drain``).

x-b553: a control mail to a busy thread worker misses live delivery
(not-confirmed), queues durable, and then waits for a PROMPT boundary
(notify-self). A worker holding one long turn never reaches one, so a merge
freeze cannot stop it. This lane lands CONTROL bodies at the next TOOL
boundary via a PreToolUse hook, on its own cursors, and never consumes
ordinary mail.
"""
from __future__ import annotations

import json

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


def _send(from_, to, body):
    from fno.bus.log import Envelope, append

    env = Envelope.new(from_=from_, to=to, kind="send", body=body)
    append(env)
    return env


def _run(capsys):
    from fno.mail.cli import cmd_control_drain

    cmd_control_drain()
    return capsys.readouterr().out


def _flag_path(form):
    from fno.bus.cursor import control_pending_dir

    return control_pending_dir() / f"{form}.flag"


def _mark(form):
    from fno.bus.cursor import mark_control_pending

    mark_control_pending(form)


# --- landing (AC1-HP) -------------------------------------------------------

def test_ac1_hp_control_body_renders_at_pretooluse(env, capsys):
    msg = _send("king", MY_HANDLE, "control: freeze - hold all merges")
    _mark(MY_HANDLE)

    payload = json.loads(_run(capsys))
    context = payload["hookSpecificOutput"]["additionalContext"]

    assert payload["hookSpecificOutput"]["hookEventName"] == "PreToolUse"
    assert msg.id in context
    assert "control: freeze" in context
    # consumed once, and the pending flag cleared
    assert _run(capsys).strip() == ""
    assert not _flag_path(MY_HANDLE).exists()


def test_ac2_hp_full_id_address_form_drains(env, capsys):
    from fno.harness_identity import session_identity_key

    full = session_identity_key(MY_SID)
    msg = _send("king", full, "control: freeze")
    _mark(full)

    context = json.loads(_run(capsys))["hookSpecificOutput"]["additionalContext"]
    assert msg.id in context
    assert not _flag_path(full).exists()


# --- ordinary mail is never consumed (AC3-CON) ------------------------------

def test_ac3_con_ordinary_mail_not_rendered_and_main_cursor_untouched(env, capsys):
    from fno.bus.cursor import read_cursor, scan_unread

    ctrl = _send("king", MY_HANDLE, "control: freeze")
    plain = _send("alice", MY_HANDLE, "ordinary status ping")
    _mark(MY_HANDLE)

    context = json.loads(_run(capsys))["hookSpecificOutput"]["additionalContext"]
    assert ctrl.id in context
    assert plain.id not in context
    # the shared cursor never moved: notify-self still owns ordinary mail
    assert scan_unread(MY_HANDLE) != []
    assert read_cursor(MY_HANDLE) is None


def test_ac4_con_mixed_interleave_keeps_ordinary_readable(env, capsys):
    from fno.bus.cursor import scan_unread
    from fno.mail.hold import cmd_notify_self

    _send("alice", MY_HANDLE, "ordinary one")
    _send("king", MY_HANDLE, "control: freeze")
    _send("bob", MY_HANDLE, "ordinary two")
    _mark(MY_HANDLE)

    context = json.loads(_run(capsys))["hookSpecificOutput"]["additionalContext"]
    assert "control: freeze" in context

    # ordinary mail still drains through the prompt boundary
    cmd_notify_self()
    prompt_ctx = capsys.readouterr().out
    assert "ordinary one" in prompt_ctx and "ordinary two" in prompt_ctx
    assert scan_unread(MY_HANDLE) == []


# --- cheap gate (AC2-ERR) ----------------------------------------------------

def test_ac2_err_no_marker_is_silent(env, capsys):
    from fno.bus.cursor import read_cursor

    _send("king", MY_HANDLE, "control: nobody marked me")
    assert _run(capsys).strip() == ""
    assert read_cursor("control:" + MY_HANDLE) is None


# --- identity guard (AC1-ERR) -------------------------------------------------

def test_ac1_err_no_identity_is_noop(tmp_path, monkeypatch, capsys):
    use_tmpdir(monkeypatch, tmp_path)
    for m in MARKERS:
        monkeypatch.delenv(m, raising=False)
    _mark(MY_HANDLE)
    from fno.mail.cli import cmd_control_drain

    cmd_control_drain()
    assert capsys.readouterr().out.strip() == ""


# --- sender side: the durable write marks pending (AC5-HP) -------------------

def test_ac5_hp_write_new_thread_marks_control_pending(env):
    from fno.inbox.store import write_new_thread

    handle = write_new_thread(
        MY_HANDLE,
        sender="king",
        kind="send",
        body="control: freeze - hold",
    )
    assert handle is not None
    assert _flag_path(MY_HANDLE).exists()


def test_ac5_hp_ordinary_write_leaves_no_flag(env):
    from fno.inbox.store import write_new_thread

    write_new_thread(MY_HANDLE, sender="alice", kind="send", body="plain status")
    assert not _flag_path(MY_HANDLE).exists()


# --- stale flag (AC3-ERR) ------------------------------------------------------

def test_ac3_err_stale_flag_without_mail_clears(env, capsys):
    _mark(MY_HANDLE)
    assert _run(capsys).strip() == ""
    assert not _flag_path(MY_HANDLE).exists()
