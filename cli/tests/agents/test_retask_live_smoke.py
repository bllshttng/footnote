"""Opt-in real-pane smoke for the spawn-to-retask path."""

from __future__ import annotations

import os
import shutil
import subprocess
import time
import uuid
from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.agents.mux_spawn import dispatch_spawn_pane
from fno.agents.retask import run_retask
from fno.agents.registry import load_registry


pytestmark = [
    pytest.mark.smoke,
    pytest.mark.skipif(
        os.environ.get("RETASK_SMOKE", "0") != "1" or shutil.which("codex") is None,
        reason="set RETASK_SMOKE=1 and ensure codex is on PATH",
    ),
]


def _settings(*, model: str, effort: str) -> SimpleNamespace:
    defaults = SimpleNamespace(
        provider="",
        model="",
        effort="",
        substrate="",
        permission_mode="",
        route="",
        account="",
        pane_group="",
        lanes=[],
    )
    profile = SimpleNamespace(
        **{**vars(defaults), "provider": "codex", "model": model, "effort": effort}
    )
    return SimpleNamespace(
        agents=SimpleNamespace(defaults=defaults, profiles={"target": profile}, max_lanes={}),
        model_routing=None,
    )


def test_real_spawned_idle_pane_retasks_with_live_readiness(monkeypatch) -> None:
    real_home = os.environ.get("RETASK_SMOKE_HOME")
    if not real_home:
        pytest.skip("set RETASK_SMOKE_HOME to the real authenticated home directory")

    repo = Path(__file__).resolve().parents[3]
    cli = ["uv", "run", "--project", str(repo / "cli"), "fno-py"]
    env = os.environ.copy()
    env.update(
        HOME=real_home,
        CODEX_HOME=str(Path(real_home) / ".codex"),
        FNO_HOME=str(Path(real_home) / ".fno"),
        FNO_AGENTS_HOME=str(Path(real_home) / ".fno" / "agents"),
    )
    env.pop("FNO_CONFIG_SEARCH_ROOT", None)
    env.pop("FNO_CLAUDE_SESSIONS_DIR", None)
    env.pop("FNO_CODEX_SESSIONS_DIR", None)
    env.pop("FNO_PANE_BINDING_WINDOW_S", None)
    env.pop("PYTEST_CURRENT_TEST", None)

    suffix = uuid.uuid4().hex[:8]
    worker = f"retask-smoke-{suffix}"
    target = f"x-{uuid.uuid4().hex[:4]}"
    model = os.environ.get("RETASK_SMOKE_MODEL")
    effort = os.environ.get("RETASK_SMOKE_EFFORT")
    if not model or not effort:
        pytest.skip("set RETASK_SMOKE_MODEL and RETASK_SMOKE_EFFORT")
    cleanup_names = [worker, f"target-{target}"]
    last_receipt: dict = {"worker": worker, "stage": "not_started"}
    session = ""
    pane = ""

    def run(args: list[str], *, timeout: float) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            args,
            cwd=repo,
            env=env,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )

    def real_runner(command, *args, **kwargs):
        kwargs.setdefault("cwd", repo)
        kwargs.setdefault("env", env)
        return subprocess.run(command, *args, **kwargs)

    try:
        from fno import rust_binary
        from fno.agents import mux_spawn

        monkeypatch.setenv("HOME", real_home)
        monkeypatch.setenv("CODEX_HOME", str(Path(real_home) / ".codex"))
        monkeypatch.setenv("FNO_HOME", str(Path(real_home) / ".fno"))
        monkeypatch.setenv(
            "FNO_AGENTS_HOME", str(Path(real_home) / ".fno" / "agents")
        )
        monkeypatch.delenv("FNO_CONFIG_SEARCH_ROOT", raising=False)
        monkeypatch.delenv("FNO_CODEX_SESSIONS_DIR", raising=False)
        dev_binary = repo / "crates" / "fno-agents" / "target" / "debug" / "fno-agents"
        assert dev_binary.is_file(), f"build the smoke binary first: {dev_binary}"
        monkeypatch.setattr(rust_binary, "resolve_installed_binary", lambda: dev_binary)
        monkeypatch.setattr(
            mux_spawn,
            "_make_codex_bind_probe",
            lambda **_kwargs: (lambda: "smoke-old-session"),
        )
        spawned = dispatch_spawn_pane(
            worker,
            "Reply only RETASK_SMOKE_IDLE. Do not call tools.",
            "codex",
            repo,
            yolo=True,
            model=model,
            effort=effort,
            runner=real_runner,
            codex_sessions_dir=Path(real_home) / ".codex" / "sessions",
        )
        last_receipt = {
            "name": spawned.name,
            "session": spawned.session,
            "pane_id": spawned.pane_id,
            "bound": spawned.bound,
        }
        assert spawned.name == worker, last_receipt
        assert spawned.bound is True, last_receipt
        session = spawned.session
        pane = str(spawned.pane_id)

        registry = Path(real_home) / ".fno" / "agents" / "registry.json"
        row = next(entry for entry in load_registry(registry) if entry.name == worker)
        assert row.mux == {"session": session, "pane_id": int(pane)}, row
        spawned_screen_state = row.screen_state
        assert spawned_screen_state is None, row

        from fno.agents.mux_spawn import _evaluate_manifest_screen, _pane_osc_title

        def live_runner(command, *args, **kwargs):
            kwargs.setdefault("cwd", repo)
            kwargs.setdefault("env", env)
            return subprocess.run(command, *args, **kwargs)

        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            frame = run(
                [
                    "fno",
                    "mux",
                    "pane",
                    "read",
                    "--session",
                    session,
                    pane,
                    "--lines",
                    "80",
                ],
                timeout=15,
            )
            osc_title = _pane_osc_title(session, int(pane), live_runner)
            verdict = (
                _evaluate_manifest_screen(
                    "codex",
                    frame.stdout,
                    live_runner,
                    osc_title=osc_title,
                )
                if frame.returncode == 0
                else {"matched": False}
            )
            last_receipt = {
                "worker": worker,
                "stage": "wait_idle",
                "spawned_screen_state": spawned_screen_state,
                "osc_title": osc_title,
                "verdict": verdict,
            }
            if (
                verdict.get("matched")
                and verdict.get("rule_id") == "idle_prompt"
                and verdict.get("state") == "idle"
            ):
                break
            time.sleep(1)
        else:
            pytest.fail(f"worker did not reach idle: {last_receipt}")

        from fno.agents import registry as registry_module
        from fno.agents import retask as retask_module

        real_run = subprocess.run
        sends: list[str] = []
        status_pending = False

        def runtime_run(command, *args, **kwargs):
            nonlocal status_pending
            tokens = [str(token) for token in command]
            if "send" in tokens and "--text" in tokens:
                text = tokens[tokens.index("--text") + 1]
                result = real_run(command, *args, **kwargs)
                sends.append(text)
                if text == "/status" and result.returncode == 0:
                    status_pending = True
                return result
            if status_pending and "read" in tokens:
                status_pending = False
                return subprocess.CompletedProcess(
                    command,
                    0,
                    stdout=f"Model: {model} (reasoning {effort}, summaries auto)",
                    stderr="",
                )
            return real_run(command, *args, **kwargs)

        monkeypatch.setenv("HOME", real_home)
        monkeypatch.setenv("FNO_HOME", str(Path(real_home) / ".fno"))
        monkeypatch.setenv(
            "FNO_AGENTS_HOME", str(Path(real_home) / ".fno" / "agents")
        )
        monkeypatch.delenv("FNO_CONFIG_SEARCH_ROOT", raising=False)
        monkeypatch.setattr(retask_module.subprocess, "run", runtime_run)
        monkeypatch.setattr(
            retask_module,
            "load_registry",
            lambda **_kwargs: [
                SimpleNamespace(name=worker, harness_session_id="smoke-new-session")
            ],
        )
        monkeypatch.setattr(
            retask_module,
            "rename_agent",
            lambda *_args, **_kwargs: SimpleNamespace(name=f"target-{target}"),
        )
        monkeypatch.setattr(
            registry_module,
            "project_verified_tier",
            lambda *_args, **_kwargs: None,
        )
        last_receipt = run_retask(
            worker,
            node=target,
            settings=_settings(model=model, effort=effort),
            model=model,
            effort=effort,
            env={},
            registry_path=registry,
        )
        assert last_receipt.get("status") == "retasked", last_receipt
        assert last_receipt.get("switch") == "skipped_same_tier", last_receipt
        assert last_receipt.get("switch_verified") is True, last_receipt
        assert last_receipt.get("target_submit_confirmed") is True, last_receipt
        assert sends[0] == "/clear"
        assert sends[1] == "/status"
        assert sends[-1].endswith(target)
    finally:
        cleanup_receipts = []
        for name in cleanup_names:
            cleaned = run([*cli, "agents", "rm", name, "--force"], timeout=60)
            cleanup_receipts.append(
                {"name": name, "returncode": cleaned.returncode, "stderr": cleaned.stderr}
            )
        if session and pane:
            run(
                ["fno", "mux", "pane", "kill", "--session", session, pane],
                timeout=30,
            )
        registry = Path(real_home) / ".fno" / "agents" / "registry.json"
        remaining = {entry.name for entry in load_registry(registry) if entry.name in cleanup_names}
        panes = run(["fno", "mux", "pane", "ls"], timeout=30)
        pane_remaining = worker in panes.stdout or (
            bool(pane) and any(line.startswith(f"{pane} ") for line in panes.stdout.splitlines())
        )
        if remaining or pane_remaining:
            pytest.fail(
                f"cleanup failed for {worker}: last={last_receipt} "
                f"cleanup={cleanup_receipts} remaining={sorted(remaining)} "
                f"pane_remaining={pane_remaining}"
            )
