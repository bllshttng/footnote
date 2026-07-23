from __future__ import annotations

import json
import threading
import time
from contextlib import contextmanager
from pathlib import Path
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from fno.agents.dispatch import DispatchAskError
from fno.agents.mux_spawn import (
    apply_opencode_variant,
    build_pane_argv,
    effort_tokens,
)

CWD = Path("/tmp")


@pytest.fixture
def runner():
    return CliRunner()


@pytest.fixture(autouse=True)
def _no_harness_markers(monkeypatch):
    for marker in (
        "CODEX_THREAD_ID",
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
    ):
        monkeypatch.delenv(marker, raising=False)


@pytest.mark.parametrize(
    "provider,value,expected",
    [
        ("claude", "high", ["--effort", "high"]),
        ("codex", "medium", ["-c", "model_reasoning_effort=medium"]),
        ("codex", "xhigh", ["-c", "model_reasoning_effort=xhigh"]),
        ("opencode", "max", []),
    ],
)
def test_effort_mapping_accepts_native_values(provider, value, expected):
    assert effort_tokens(provider, value) == expected


@pytest.mark.parametrize(
    "provider,value",
    [("codex", "max"), ("claude", "minimal"), ("gemini", "high"), ("agy", "low"), ("claude", "")],
)
def test_effort_mapping_fails_closed(provider, value):
    with pytest.raises(DispatchAskError) as exc:
        effort_tokens(provider, value)
    assert exc.value.exit_code == 2


def test_pane_argv_threads_effort_and_unset_is_noop():
    claude = build_pane_argv("claude", "hi", CWD, False, "uuid", effort="high")
    codex = build_pane_argv("codex", "hi", CWD, False, None, effort="medium")
    assert claude[-3:] == ["--effort", "high", "hi"]
    assert ["-c", "model_reasoning_effort=medium"] == codex[5:7]
    assert build_pane_argv("claude", "hi", CWD, False, "uuid", effort=None) == build_pane_argv(
        "claude", "hi", CWD, False, "uuid"
    )


@pytest.mark.parametrize("corrupt", ["{", "[]", "null"])
def test_opencode_variant_write_and_corrupt_degrade(tmp_path, capsys, corrupt):
    state = tmp_path / "model.json"
    state.write_text(json.dumps({"variant": {"other/model": "low"}}))
    apply_opencode_variant("opencode/glm-4.7-free", "high", state_path=state)
    assert json.loads(state.read_text())["variant"] == {
        "other/model": "low",
        "opencode/glm-4.7-free": "high",
    }
    state.write_text(corrupt)
    before = state.read_bytes()
    apply_opencode_variant("opencode/glm-4.7-free", "high", state_path=state)
    assert state.read_bytes() == before
    assert "warning" in capsys.readouterr().err.lower()


def test_opencode_variant_serializes_full_read_modify_write(tmp_path, monkeypatch):
    from fno.agents import mux_spawn

    state = tmp_path / "model.json"
    state.write_text('{"variant":{}}')
    active = 0
    max_active = 0
    guard = threading.Lock()
    real_loads = json.loads

    def slow_loads(value):
        nonlocal active, max_active
        with guard:
            active += 1
            max_active = max(max_active, active)
        time.sleep(0.05)
        try:
            return real_loads(value)
        finally:
            with guard:
                active -= 1

    monkeypatch.setattr(mux_spawn.json, "loads", slow_loads)
    threads = [
        threading.Thread(
            target=apply_opencode_variant,
            args=(f"provider/model-{i}", "high"),
            kwargs={"state_path": state},
        )
        for i in range(2)
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert max_active == 1
    assert json.loads(state.read_text())["variant"] == {
        "provider/model-0": "high",
        "provider/model-1": "high",
    }


def test_opencode_variant_is_not_written_before_collision_check(tmp_path, monkeypatch):
    from fno.agents import mux_spawn

    applied = []

    @contextmanager
    def unlocked(*args, **kwargs):
        yield

    monkeypatch.setattr(mux_spawn, "hold_agent_lock", unlocked)
    monkeypatch.setattr(mux_spawn, "load_registry", lambda: [SimpleNamespace(name="taken")])
    monkeypatch.setattr(mux_spawn.paths, "agents_registry_path", lambda: tmp_path / "registry.json")
    monkeypatch.setattr(mux_spawn, "apply_opencode_variant", lambda *args, **kwargs: applied.append(args))

    with pytest.raises(DispatchAskError):
        mux_spawn.dispatch_spawn_pane(
            "taken", "hi", "opencode", tmp_path, effort="high"
        )
    assert applied == []


def test_opencode_variant_cleans_temp_file_when_replace_fails(tmp_path, monkeypatch):
    from fno.agents import mux_spawn

    state = tmp_path / "model.json"
    monkeypatch.setattr(mux_spawn.os, "replace", lambda *args: (_ for _ in ()).throw(OSError("no space")))
    apply_opencode_variant("provider/model", "high", state_path=state)
    assert sorted(path.name for path in tmp_path.iterdir()) == ["locks"]


def test_claude_python_build_argv_threads_effort():
    from fno.agents.providers.claude import _build_argv

    assert _build_argv("w", "hi", False, effort="high") == [
        "claude",
        "--bg",
        "--name",
        "w",
        "--effort",
        "high",
        "hi",
    ]


def test_codex_python_create_threads_effort(monkeypatch, tmp_path):
    from fno.agents.providers import codex

    captured = {}

    def fake_run_codex(**kwargs):
        captured.update(kwargs)
        return codex.CodexResult(0, "session", "ok", 1)

    monkeypatch.setattr(codex, "_run_codex", fake_run_codex)
    codex.create(
        cwd=tmp_path,
        prompt="hi",
        from_name="parent",
        yolo=False,
        output_path=tmp_path / "out.jsonl",
        reasoning_effort="high",
    )
    argv = captured["argv"]
    assert argv[1:3] == ["-c", "model_reasoning_effort=high"]


def test_claude_python_headless_threads_effort(monkeypatch, tmp_path):
    from types import SimpleNamespace
    from fno.agents.providers import claude

    captured = {}

    def fake_run(argv, **kwargs):
        captured["argv"] = argv
        return SimpleNamespace(returncode=0, stdout="reply", stderr="")

    monkeypatch.setattr(claude, "_subprocess_run", fake_run)
    result = claude.headless_create("hi", tmp_path, effort="high")
    assert captured["argv"] == [
        "claude",
        "-p",
        "--dangerously-skip-permissions",
        "--effort",
        "high",
        "hi",
    ]
    assert result.stdout == "reply"


def _stub_pane_path(monkeypatch):
    from fno.agents import mux_spawn, spawn_gate

    received = {}

    class Gate:
        def release(self):
            pass

    def fake_dispatch(**kwargs):
        received.update(kwargs)
        return mux_spawn.MuxSpawnResult(
            name=kwargs["name"],
            provider=kwargs["provider"],
            session="session",
            pane_id=1,
            child_pid=None,
            session_uuid=None,
        )

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *args, **kwargs: Gate())
    monkeypatch.setattr(mux_spawn, "resolve_provenance", lambda *args, **kwargs: None)
    monkeypatch.setattr(mux_spawn, "dispatch_spawn_pane", fake_dispatch)
    return received


def test_cli_threads_effort_to_pane_dispatch(runner, monkeypatch):
    received = _stub_pane_path(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "worker", "hi", "--harness", "claude", "--effort", "high"],
    )
    assert result.exit_code == 0, result.output
    assert received["effort"] == "high"


def test_cli_rejects_unmappable_effort_before_spawn(runner, monkeypatch):
    received = _stub_pane_path(monkeypatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        ["spawn", "worker", "hi", "--harness", "codex", "--effort", "max"],
    )
    assert result.exit_code == 2
    assert "codex supports" in result.output
    assert received == {}


def test_cli_threads_effort_to_bg_dispatch(runner, monkeypatch):
    from fno.agents import dispatch, spawn_gate

    received = {}

    class Gate:
        def release(self):
            pass

    def fake_dispatch(**kwargs):
        received.update(kwargs)
        return dispatch.SpawnResult(
            kind="created",
            name=kwargs["name"],
            provider="claude",
            short_id="abcd1234",
        )

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *args, **kwargs: Gate())
    monkeypatch.setattr(dispatch, "dispatch_spawn", fake_dispatch)
    from fno.agents.cli import agents_app

    result = runner.invoke(
        agents_app,
        [
            "spawn",
            "worker",
            "hi",
            "--harness",
            "claude",
            "--substrate",
            "bg",
            "--effort",
            "high",
        ],
    )
    assert result.exit_code == 0, result.output
    assert received["effort"] == "high"
