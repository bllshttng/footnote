"""`--permission-mode` passthrough across the spawn surfaces (x-dfa4).

Covers the per-provider mapping table (accepted value -> exact argv; unmappable
-> fail-closed error), the claude `--yolo` -> bypassPermissions fix, the
`--permission-mode`/`--yolo` mutual exclusion, and the config default that
applies to autonomous dispatchers only. No test performs a live spawn - the pane
dispatch is stubbed and the autonomous dispatchers' subprocess is mocked.
"""

from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from fno.agents.dispatch import DispatchAskError
from fno.agents.mux_spawn import build_pane_argv, permission_pane_tokens

CWD = Path("/tmp")


# --- mapping table (permission_pane_tokens) --------------------------------


@pytest.mark.parametrize(
    "provider,mode,expected",
    [
        ("claude", "acceptEdits", ["--permission-mode", "acceptEdits"]),
        ("claude", "plan", ["--permission-mode", "plan"]),
        ("gemini", "plan", ["--approval-mode", "plan"]),
        ("gemini", "yolo", ["--yolo"]),
        ("codex", "full-auto", ["--full-auto"]),
        ("codex", "yolo", ["--dangerously-bypass-approvals-and-sandbox"]),
        (
            "codex",
            "workspace-write:on-request",
            ["--sandbox", "workspace-write", "--ask-for-approval", "on-request"],
        ),
        ("opencode", "auto", ["--auto"]),
        ("agy", "skip", []),  # argv already carries --dangerously-skip-permissions
    ],
)
def test_mapping_accepts_provider_native_values(provider, mode, expected):
    assert permission_pane_tokens(provider, mode) == expected


@pytest.mark.parametrize(
    "provider,mode",
    [
        ("opencode", "acceptEdits"),  # AC3-ERR: only 'auto' maps
        ("agy", "plan"),  # only 'skip' maps
        ("codex", "bogus"),  # not a shortcut or colon form
        ("codex", "workspace-write"),  # colon form needs both axes
        ("claude", ""),  # empty value required
    ],
)
def test_mapping_fail_closed_on_unmappable(provider, mode):
    with pytest.raises(DispatchAskError) as exc:
        permission_pane_tokens(provider, mode)
    assert exc.value.exit_code == 2


# --- build_pane_argv integration -------------------------------------------


def test_claude_pane_permission_mode_passthrough():
    """AC1/AC2-HP flavor: explicit mode rides as exact passthrough, no skip flag."""
    argv = build_pane_argv("claude", "hi", CWD, False, "uuid", None, "acceptEdits")
    assert "--permission-mode" in argv and "acceptEdits" in argv
    assert "--dangerously-skip-permissions" not in argv


def test_claude_pane_yolo_maps_to_bypass_permissions():
    """AC4-HP: claude --yolo now means bypassPermissions (was a no-op)."""
    argv = build_pane_argv("claude", "hi", CWD, True, "uuid", None)
    assert argv[-3:] == ["--permission-mode", "bypassPermissions", "hi"]


def test_codex_pane_colon_two_axis_form():
    """AC2-HP: codex <sandbox>:<approval> -> --sandbox X --ask-for-approval Y."""
    argv = build_pane_argv(
        "codex", "hi", CWD, False, None, None, "workspace-write:on-request"
    )
    assert argv[:6] == [
        "codex",
        "-C",
        str(CWD),
        "--sandbox",
        "workspace-write",
        "--ask-for-approval",
    ]


@pytest.mark.parametrize("provider", ["claude", "codex", "gemini", "opencode", "agy"])
def test_unset_is_byte_identical(provider):
    """AC7-EDGE: no mode + no yolo -> byte-identical to pre-feature argv."""
    with_arg = build_pane_argv(provider, "hi", CWD, False, "uuid", None, None)
    without = build_pane_argv(provider, "hi", CWD, False, "uuid", None)
    assert with_arg == without


# --- cmd_spawn CLI wiring ---------------------------------------------------


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture(autouse=True)
def _no_harness_markers(monkeypatch):
    for m in (
        "CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID"
    ):
        monkeypatch.delenv(m, raising=False)


def _stub_pane_path(monkeypatch) -> dict:
    received: dict = {}
    from fno.agents import mux_spawn, spawn_gate

    class _Gate:
        def release(self) -> None:
            pass

    def fake_dispatch_spawn_pane(**kwargs):
        received.update(kwargs)
        return mux_spawn.MuxSpawnResult(
            name=kwargs["name"],
            provider=kwargs["provider"],
            session="sess-1",
            pane_id=1,
            child_pid=None,
            session_uuid=None,
        )

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    monkeypatch.setattr(mux_spawn, "resolve_provenance", lambda *a, **k: None)
    monkeypatch.setattr(mux_spawn, "dispatch_spawn_pane", fake_dispatch_spawn_pane)
    return received


def test_mutual_exclusion_yolo_and_permission_mode(runner, monkeypatch):
    """AC5-ERR: --yolo + --permission-mode is one knob at a time."""
    _stub_pane_path(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "claude", "--yolo",
         "--permission-mode", "plan"],
    )
    assert result.exit_code == 2
    assert "mutually exclusive" in result.output


def test_permission_mode_reaches_pane_dispatch(runner, monkeypatch):
    """The explicit flag threads to dispatch_spawn_pane on the pane substrate."""
    received = _stub_pane_path(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "claude",
         "--permission-mode", "acceptEdits"],
    )
    assert result.exit_code == 0, result.output
    assert received["permission_mode"] == "acceptEdits"
    # Receipt names the applied mode (Locked Decision 5).
    assert json.loads(result.output)["permission_mode"] == "acceptEdits"


@pytest.mark.parametrize(
    "extra_args",
    [
        # explicit non-bg substrate: a bare --resume now IMPLIES bg (x-f76e),
        # so the guard is exercised by pinning a non-bg lane explicitly.
        ["--substrate", "pane"],
        ["--substrate", "bg", "--provider", "codex"],  # bg but non-claude
    ],
)
def test_resume_requires_claude_bg(runner, monkeypatch, extra_args):
    """US4: --resume continues a claude --bg transcript, so it is rejected on any
    non-(claude, bg) lane with exit 2 before dispatch. The resume value must be a
    real session uuid: the x-f76e front-door normalizer validates the shape first."""
    _stub_pane_path(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--resume",
         "6501096a-1111-2222-3333-444455556666", *extra_args],
    )
    assert result.exit_code == 2
    assert "--resume requires --substrate bg" in result.output


def test_bg_permission_mode_non_claude_fails_closed(runner, monkeypatch):
    """codex bg/headless via the Python fallback refuses --permission-mode
    (its one-shot lane hardcodes its bypass); mirrors the Rust guard."""
    _stub_pane_path(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "codex", "--substrate", "headless",
         "--permission-mode", "acceptEdits"],
    )
    assert result.exit_code == 2
    assert "not supported" in result.output and "pane" in result.output


def test_bg_permission_mode_claude_honored_via_python(runner, monkeypatch):
    """claude bg via the Python fallback HONORS --permission-mode (it threads to
    _claude_create_path -> bg_create), never a hard-fail. Regression guard for
    the availability bug where a config default hard-failed autonomous dispatch."""
    from fno.agents import dispatch, spawn_gate

    captured: dict = {}

    class _Gate:
        def release(self) -> None:
            pass

    def fake_dispatch_spawn(**kwargs):
        captured.update(kwargs)
        return dispatch.SpawnResult(
            kind="created", name=kwargs["name"], provider="claude", short_id="abcd1234"
        )

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    monkeypatch.setattr("fno.agents.dispatch.dispatch_spawn", fake_dispatch_spawn)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "claude", "--substrate", "bg",
         "--permission-mode", "acceptEdits"],
    )
    assert result.exit_code == 0, result.output
    assert captured["permission_mode"] == "acceptEdits"
    # Locked Decision 5 / Rust parity: the fallback bg receipt names the mode.
    assert json.loads(result.output.splitlines()[0])["permission_mode"] == "acceptEdits"


def test_bg_yolo_receipt_names_bypass_via_python(runner, monkeypatch):
    """--yolo on the Python claude bg fallback names bypassPermissions in the
    receipt (audit parity with the Rust bg receipt)."""
    from fno.agents import dispatch, spawn_gate

    class _Gate:
        def release(self) -> None:
            pass

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    monkeypatch.setattr(
        "fno.agents.dispatch.dispatch_spawn",
        lambda **kw: dispatch.SpawnResult(
            kind="created", name=kw["name"], provider="claude", short_id="abcd1234"
        ),
    )
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "w1", "hi", "--provider", "claude", "--substrate", "bg", "--yolo"],
    )
    assert result.exit_code == 0, result.output
    assert json.loads(result.output.splitlines()[0])["permission_mode"] == "bypassPermissions"


def test_claude_python_build_argv_threads_permission_mode():
    """The Python claude bg argv builder mirrors Rust: --permission-mode rides
    between --name and --model; unset is byte-identical."""
    from fno.agents.providers.claude import _build_argv

    argv = _build_argv("w1", "hi", False, None, "acceptEdits")
    assert argv == ["claude", "--bg", "--name", "w1", "--permission-mode",
                    "acceptEdits", "hi"]
    assert _build_argv("w1", "hi", False, None, None) == _build_argv("w1", "hi", False)


# --- AC8: config default applies to autonomous dispatchers only -------------


def _fake_settings(mode: str):
    return SimpleNamespace(agents=SimpleNamespace(spawn_permission_mode=mode))


def _capture_spawn(monkeypatch, module):
    """Mock the spawn subprocess at ``module.subprocess.run``; return the cmd."""
    captured: dict = {}

    def fake_run(cmd, **kwargs):
        captured["cmd"] = cmd
        return SimpleNamespace(
            returncode=0, stdout='{"name":"w","short_id":"abcd1234"}', stderr=""
        )

    monkeypatch.setattr(module.subprocess, "run", fake_run)
    return captured


def test_advance_worker_reads_config_default(monkeypatch):
    """AC8-FR: an autonomous /target dispatch picks up the config default."""
    from fno.backlog import advance

    captured = _capture_spawn(monkeypatch, advance)
    monkeypatch.setattr(
        "fno.config.load_settings", lambda: _fake_settings("acceptEdits")
    )
    advance._spawn_worker("x-test", None, "slug")
    assert "--permission-mode" in captured["cmd"]
    i = captured["cmd"].index("--permission-mode")
    assert captured["cmd"][i + 1] == "acceptEdits"


def test_advance_worker_explicit_flag_wins_over_config(monkeypatch):
    from fno.backlog import advance

    captured = _capture_spawn(monkeypatch, advance)
    monkeypatch.setattr("fno.config.load_settings", lambda: _fake_settings("plan"))
    advance._spawn_worker("x-test", None, "slug", permission_mode="acceptEdits")
    i = captured["cmd"].index("--permission-mode")
    assert captured["cmd"][i + 1] == "acceptEdits"


def test_advance_worker_unset_config_is_unchanged(monkeypatch):
    from fno.backlog import advance

    captured = _capture_spawn(monkeypatch, advance)
    monkeypatch.setattr("fno.config.load_settings", lambda: _fake_settings(""))
    advance._spawn_worker("x-test", None, "slug")
    assert "--permission-mode" not in captured["cmd"]


def test_think_worker_reads_config_default(monkeypatch):
    from fno.provenance import spawn_think

    captured = _capture_spawn(monkeypatch, spawn_think)
    monkeypatch.setattr(
        spawn_think, "_settings_for", lambda root: _fake_settings("plan")
    )
    spawn_think._spawn_think_worker("x-test", "prompt", None, "slug")
    assert "--permission-mode" in captured["cmd"]
    i = captured["cmd"].index("--permission-mode")
    assert captured["cmd"][i + 1] == "plan"


# --- bash surface: /agent spawn (spawn.sh) ----------------------------------

import os  # noqa: E402
import shutil  # noqa: E402
import stat  # noqa: E402
import subprocess as _sp  # noqa: E402


@pytest.mark.skipif(shutil.which("jq") is None, reason="spawn.sh needs jq")
def test_spawn_sh_forwards_permission_mode(tmp_path):
    """AC6 flavor: /agent spawn (spawn.sh) forwards --permission-mode verbatim."""
    repo = Path(__file__).resolve().parents[3]
    script = repo / "skills" / "agent" / "scripts" / "spawn.sh"
    assert script.exists()

    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    argv_file = tmp_path / "fno-argv.txt"
    fake = bin_dir / "fno"
    fake.write_text(
        "#!/bin/sh\n"
        # Answer the collision probe (`fno agents list`) with an empty roster,
        # and the worktree-ensure with an empty path (so spawn.sh skips setup and
        # never treats the spawn receipt as a worktree dir). Record the spawn argv.
        'case "$*" in\n'
        '  *"agents list"*) printf \'{"agents":[]}\\n\'; exit 0 ;;\n'
        '  *"worktree ensure"*) exit 0 ;;\n'
        'esac\n'
        f'printf "%s\\n" "$@" > "{argv_file}"\n'
        'printf \'{"short_id":"deadbeef"}\\n\'\n'
    )
    fake.chmod(fake.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)

    env = dict(os.environ)
    env["PATH"] = f"{bin_dir}:{env['PATH']}"
    res = _sp.run(
        ["bash", str(script), "--name", "w1", "--provider", "claude",
         "--message", "hi", "--permission-mode", "acceptEdits"],
        capture_output=True, text=True, env=env, timeout=30, cwd=str(tmp_path),
    )
    assert res.returncode == 0, res.stdout + res.stderr
    forwarded = argv_file.read_text().splitlines()
    assert "--permission-mode" in forwarded
    i = forwarded.index("--permission-mode")
    assert forwarded[i + 1] == "acceptEdits"
