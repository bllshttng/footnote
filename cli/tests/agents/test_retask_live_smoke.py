"""Opt-in real-pane smoke for the spawn-to-retask path."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import time
import uuid
from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.agents.retask import execute_retask, resolve_target_coordinate
from fno.agents.registry import load_registry


pytestmark = [
    pytest.mark.smoke,
    pytest.mark.skipif(
        os.environ.get("RETASK_SMOKE", "0") != "1" or shutil.which("claude") is None,
        reason="set RETASK_SMOKE=1 and ensure claude is on PATH",
    ),
]


def _receipt(proc: subprocess.CompletedProcess[str]) -> dict:
    for line in reversed(proc.stdout.splitlines()):
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict):
            return value
    pytest.fail(
        f"command emitted no JSON receipt: rc={proc.returncode} "
        f"stdout={proc.stdout!r} stderr={proc.stderr!r}"
    )


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
        **{**vars(defaults), "provider": "claude", "model": model, "effort": effort}
    )
    return SimpleNamespace(
        agents=SimpleNamespace(defaults=defaults, profiles={"target": profile}, max_lanes={}),
        model_routing=None,
    )


def test_real_spawned_idle_pane_retasks_with_live_readiness() -> None:
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
    account = os.environ.get("RETASK_SMOKE_ACCOUNT")
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

    try:
        spawn_args = [
            *cli,
            "agents",
            "spawn",
            "--force",
            "--name",
            worker,
            "--harness",
            "claude",
            "--substrate",
            "pane",
            "--cwd",
            str(repo),
        ]
        if account:
            spawn_args.extend(["--account", account])
        spawn_args.extend(
            [
                "--model",
                model,
                "--effort",
                effort,
                "--timeout",
                "180",
                "Reply only RETASK_SMOKE_IDLE. Do not call tools.",
            ]
        )
        spawned = run(spawn_args, timeout=210)
        assert spawned.returncode == 0, spawned.stderr
        last_receipt = _receipt(spawned)
        assert last_receipt.get("name") == worker, last_receipt
        session = str(last_receipt.get("mux_session") or "")
        pane = str(last_receipt.get("pane_id") or "")
        assert session and pane, last_receipt

        registry = Path(real_home) / ".fno" / "agents" / "registry.json"
        row = next(entry for entry in load_registry(registry) if entry.name == worker)
        assert row.mux == {"session": session, "pane_id": int(pane)}, row
        spawned_screen_state = row.screen_state
        assert spawned_screen_state is None, row

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
            idle = (
                frame.returncode == 0
                and "❯" in frame.stdout
                and "Login expired" not in frame.stdout
                and "Not logged in" not in frame.stdout
            )
            verdict = {
                "matched": idle,
                "rule_id": "live_prompt_box" if idle else None,
                "state": "idle" if idle else None,
            }
            last_receipt = {
                "worker": worker,
                "stage": "wait_idle",
                "spawned_screen_state": spawned_screen_state,
                "verdict": verdict,
            }
            if (
                verdict.get("matched")
                and verdict.get("rule_id") == "live_prompt_box"
                and verdict.get("state") == "idle"
            ):
                break
            time.sleep(1)
        else:
            pytest.fail(f"worker did not reach idle: {last_receipt}")

        sends: list[tuple[str, bool]] = []
        frames = iter(
            [
                frame.stdout,
                f"Model: {model} (reasoning {effort}, summaries auto)",
            ]
        )

        def send(text: str, submit: bool) -> bool:
            command = [
                "fno",
                "mux",
                "pane",
                "send",
                "--session",
                session,
                pane,
                "--text",
                text,
                "--raw",
            ]
            if submit:
                command.append("--submit")
            sent = run(command, timeout=30)
            sends.append((text, submit))
            return sent.returncode == 0

        def settle() -> None:
            run(
                [
                    "fno",
                    "mux",
                    "pane",
                    "wait",
                    "--session",
                    session,
                    pane,
                    "--quiet-ms",
                    "400",
                    "--timeout",
                    "8",
                ],
                timeout=15,
            )

        target_coordinate = resolve_target_coordinate(
            target,
            settings=_settings(model=model, effort=effort),
            model=model,
            effort=effort,
            env={},
        )
        last_receipt = execute_retask(
            row,
            target_coordinate,
            node=target,
            read_frame=lambda: next(frames),
            ready_frame=lambda frame_text: {
                "matched": "❯" in frame_text,
                "rule_id": "live_prompt_box",
                "state": "idle",
            },
            send=send,
            restamp=lambda: "smoke-new-session",
            rename=lambda _name: f"target-{target}",
            project_tier=lambda _model, _effort: None,
            settle=settle,
        )
        assert last_receipt.get("status") == "retasked", last_receipt
        assert last_receipt.get("switch") == "skipped_same_tier", last_receipt
        assert last_receipt.get("switch_verified") is True, last_receipt
        assert last_receipt.get("target_submit_confirmed") is True, last_receipt
        assert sends[0] == ("/clear", True)
        assert sends[1] == ("/status", True)
        assert sends[-1][0].endswith(target)
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
