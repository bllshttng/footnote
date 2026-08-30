from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest


def test_cursor_agent_capability_contract_is_pane_bound_and_callee_minted():
    from fno.agents.harness_map import capabilities, render_session_argv

    caps = capabilities("cursor-agent")

    assert caps["thread"] is False
    assert caps["ready_marker"] == "idle_follow_up"
    assert caps["ready_rule_ids"] == ["idle_follow_up"]
    assert caps["send_keys_enter_delay_ms"] == 0
    assert caps["session_binding"] == {
        "strategy": "callee-minted-read-back",
        "required": True,
        "timeout_ms": 60000,
    }
    assert render_session_argv(
        "cursor-agent", "interactive_resume", "fadad56b-8008-45f5-b809-f9fab7074534"
    ) == ["cursor-agent", "--resume", "fadad56b-8008-45f5-b809-f9fab7074534"]


def test_cursor_agent_resume_rejects_empty_and_truncated_ids():
    from fno.agents.harnesses import cursor_agent

    with pytest.raises(cursor_agent.CursorAgentSessionError, match="empty"):
        cursor_agent.resume_argv("")

    with pytest.raises(cursor_agent.CursorAgentSessionError) as caught:
        cursor_agent.resume_argv("74db359a")
    message = str(caught.value)
    assert "8 hex characters" in message
    assert "an fno session handle, not a chat id" in message
    assert "cursor-agent create-chat" in message


def test_cursor_agent_resume_and_attach_keep_full_id_and_trust():
    from fno.agents.harnesses import cursor_agent

    chat_id = "fadad56b-8008-45f5-b809-f9fab7074534"
    assert cursor_agent.resume_argv(chat_id) == [
        "cursor-agent", "--resume", chat_id, "--trust"
    ]
    assert cursor_agent.attach_argv(chat_id) == cursor_agent.resume_argv(chat_id)


def test_cursor_agent_create_chat_reads_one_line_then_kills_process(monkeypatch):
    from fno.agents.harnesses import cursor_agent

    read_fd, write_fd = os.pipe()
    os.write(write_fd, b"fadad56b-8008-45f5-b809-f9fab7074534\n")
    os.close(write_fd)

    class FakeProcess:
        def __init__(self):
            self.stdout = os.fdopen(read_fd, "rb")
            self.killed = False
            self.waited = False

        def kill(self):
            self.killed = True

        def wait(self, timeout):
            self.waited = True
            return -9

    process = FakeProcess()
    monkeypatch.setattr(cursor_agent, "_subprocess_popen", lambda *a, **k: process)

    assert cursor_agent.create_chat("/tmp") == "fadad56b-8008-45f5-b809-f9fab7074534"
    assert process.killed
    assert process.waited


def test_cursor_agent_pane_argv_is_trusted_and_never_native_worktree():
    from fno.agents.mux_spawn import build_pane_argv

    chat_id = "fadad56b-8008-45f5-b809-f9fab7074534"
    argv = build_pane_argv(
        "cursor-agent", "", Path("/tmp/worktree"), False, chat_id
    )
    assert "--trust" in argv
    assert chat_id in argv
    assert not any(token in {"-w", "--worktree", "--worktree-base"} for token in argv)


def test_cursor_agent_permission_and_effort_mappings_fail_closed():
    from fno.agents.mux_spawn import DispatchAskError, effort_tokens, permission_pane_tokens

    assert permission_pane_tokens("cursor-agent", "force") == ["--force"]
    with pytest.raises(DispatchAskError, match="unmappable"):
        permission_pane_tokens("cursor-agent", "auto")
    with pytest.raises(DispatchAskError, match="no --effort flag"):
        effort_tokens("cursor-agent", "high")


def test_cursor_agent_native_worktree_location_is_forbidden():
    rule = (
        Path(__file__).resolve().parents[3]
        / ".claude"
        / "rules"
        / "worktrees.md"
    ).read_text(encoding="utf-8")

    assert "~/.cursor/worktrees" in rule


def test_cursor_agent_reaps_detached_worker_server(monkeypatch):
    from fno.agents.harnesses import cursor_agent

    class FakeProcess:
        pid = 731

        def __init__(self):
            self.info = {
                "cmdline": [
                    "/Users/test/.local/share/cursor-agent/index.js",
                    "worker-server",
                ]
            }
            self.terminated = False

        def terminate(self):
            self.terminated = True

        def wait(self, timeout):
            assert timeout == 2
            assert self.terminated

    process = FakeProcess()

    class FakePsutil:
        class AccessDenied(Exception):
            pass

        class NoSuchProcess(Exception):
            pass

        class ZombieProcess(Exception):
            pass

        class TimeoutExpired(Exception):
            pass

        @staticmethod
        def process_iter(attrs):
            assert attrs == ["cmdline"]
            return [process]

    monkeypatch.setitem(sys.modules, "psutil", FakePsutil)

    assert cursor_agent.reap_detached_worker_servers() == 1
    assert process.terminated


def test_cursor_agent_is_named_in_spawn_help():
    from typer.testing import CliRunner

    from fno.agents.cli import agents_app

    result = CliRunner().invoke(agents_app, ["spawn", "--help"])

    assert result.exit_code == 0, result.output
    assert "cursor-agent" in result.output
