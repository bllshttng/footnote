from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest


def test_cursor_agent_capability_contract_is_pty_hosted_and_callee_minted():
    from fno.agents.harness_map import capabilities, render_session_argv

    caps = capabilities("cursor-agent")

    # TRUE behind the live keeper journey (journey wk-cursor,
    # cli/scripts/smoke/cursor-agent-keeper-journey.py, first green
    # 2026-09-01): the keeper hosts the TUI across supervisor death and a
    # fresh turn recalls a prior turn's codeword. The seat derives from the
    # spawn claim, which carries that journey.
    assert caps["features"]["spawn"]["state"] == "native"
    assert caps["ready_marker"] == "idle_plan_build"
    assert caps["ready_rule_ids"] == ["idle_plan_build"]
    assert caps["send_keys_enter_delay_ms"] == 0
    assert caps["session_binding"] == {
        "strategy": "callee-minted-read-back",
        "required": True,
        "timeout_ms": 60000,
    }
    # The declared form IS the launch argv: --trust rides it because an
    # untrusted cwd refuses with Workspace Trust Required, and fno always
    # spawns into a worktree it just created.
    assert render_session_argv(
        "cursor-agent", "interactive_resume", "fadad56b-8008-45f5-b809-f9fab7074534"
    ) == [
        "cursor-agent",
        "--resume",
        "fadad56b-8008-45f5-b809-f9fab7074534",
        "--trust",
    ]


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
            self.stderr = None
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


def test_cursor_agent_create_chat_keeps_stderr_out_of_uuid_stream(monkeypatch):
    from fno.agents.harnesses import cursor_agent

    stdout_read, stdout_write = os.pipe()
    stderr_read, stderr_write = os.pipe()
    os.write(stdout_write, b"fadad56b-8008-45f5-b809-f9fab7074534\n")
    os.write(stderr_write, b"warning before uuid\n")
    os.close(stdout_write)
    os.close(stderr_write)

    class FakeProcess:
        def __init__(self):
            self.stdout = os.fdopen(stdout_read, "rb")
            self.stderr = os.fdopen(stderr_read, "rb")
            self.killed = False

        def kill(self):
            self.killed = True

        def wait(self, timeout):
            return -9

    process = FakeProcess()
    captured = {}

    def fake_popen(*args, **kwargs):
        captured.update(kwargs)
        return process

    monkeypatch.setattr(cursor_agent, "_subprocess_popen", fake_popen)

    assert cursor_agent.create_chat("/tmp") == "fadad56b-8008-45f5-b809-f9fab7074534"
    assert captured["stderr"] is subprocess.PIPE


def test_cursor_agent_registry_session_id_mapping_is_explicit():
    from fno.agents.registry import HARNESS_SESSION_ID_FIELDS

    assert HARNESS_SESSION_ID_FIELDS["cursor-agent"] == "harness_session_id"


def test_cursor_agent_thread_dispatch_resolves_on_the_journey_backed_bit():
    """The thread bit reads true behind the live keeper journey
    (cli/scripts/smoke/cursor-agent-keeper-journey.py), so a one-shot
    dispatch resolves onto the keeper lane and the row's lane answer is
    keeper. The autonomous /target template still refuses at the loop gate:
    loop_participation stays extension until a stop-hook firing marker is
    proven, so a looping command cannot resolve."""
    from fno.agents.harness_map import (
        DispatchResolveError,
        capabilities,
        resolve_dispatch,
        thread_lane,
    )

    assert thread_lane("cursor-agent") == "keeper"
    from fno.agents.harness_map import thread_seatable

    assert thread_seatable("cursor-agent") is True
    resolved = resolve_dispatch(
        harness="cursor-agent",
        substrate="thread",
        command="cursor-agent --version",
    )
    assert resolved["substrate"] == "thread"
    assert resolved["thread"] is True
    with pytest.raises(DispatchResolveError, match="Dispatch a one-shot instead"):
        resolve_dispatch(harness="cursor-agent", substrate="thread")


def test_cursor_agent_pane_argv_is_trusted_and_never_native_worktree():
    from fno.agents.mux_spawn import build_pane_argv

    chat_id = "fadad56b-8008-45f5-b809-f9fab7074534"
    argv = build_pane_argv(
        "cursor-agent", "", Path("/tmp/worktree"), False, chat_id
    )
    assert argv.count("--trust") == 1, "the declared form carries it once, never duplicated"
    assert chat_id in argv
    assert not any(token in {"-w", "--worktree", "--worktree-base"} for token in argv)


@pytest.mark.parametrize("flag", ["-w", "--worktree", "--worktree-base"])
def test_cursor_agent_refuses_native_worktree_passthrough(flag):
    from fno.agents.mux_spawn import DispatchAskError, build_pane_argv

    chat_id = "fadad56b-8008-45f5-b809-f9fab7074534"
    with pytest.raises(DispatchAskError, match="native worktree"):
        build_pane_argv(
            "cursor-agent",
            "",
            Path("/tmp/worktree"),
            False,
            chat_id,
            passthrough=[flag, "native"],
        )


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


def test_cursor_agent_worker_server_selector_is_exclusive_to_owner_tree():
    from fno.agents.harnesses import cursor_agent

    rows = [
        (100, 1, "/bin/zsh"),
        (200, 100, "/Users/test/cursor-agent worker-server"),
        (300, 999, "/Users/test/cursor-agent worker-server"),
        (400, 200, "/Users/test/cursor-agent worker-server"),
    ]

    assert cursor_agent.select_owned_worker_server_pids(rows, owner_pid=100) == [200, 400]


def test_cursor_agent_reaper_refuses_unreadable_captured_identity(monkeypatch):
    from fno.agents.harnesses import cursor_agent

    class FakeProcess:
        pid = 731

        def cmdline(self):
            return ["/Users/test/cursor-agent", "worker-server"]

        def terminate(self):
            raise AssertionError("an unreadable identity must not be signaled")

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
        def Process(pid):
            assert pid == 731
            return FakeProcess()

    monkeypatch.setitem(sys.modules, "psutil", FakePsutil)
    monkeypatch.setattr(cursor_agent, "_process_start_token", lambda pid, psutil: None)

    with pytest.raises(RuntimeError, match="could not be confirmed"):
        cursor_agent.reap_detached_worker_servers(
            [cursor_agent.CursorWorkerServerHandle(pid=731, start_time=7)]
        )


def test_cursor_agent_is_named_in_spawn_help():
    from typer.testing import CliRunner

    from fno.agents.cli import agents_app

    result = CliRunner().invoke(agents_app, ["spawn", "--help"])

    assert result.exit_code == 0, result.output
    assert "cursor-agent" in result.output
