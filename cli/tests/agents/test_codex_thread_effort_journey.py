"""The codex thread effort journey (opt-in, ``FNO_CODEX_LIVE=1``).

An operator spawn carrying ``--effort`` must land that effort on a REAL
app-server thread - not in a refusal, not in a dropped field. The positive
marker is the vendor's own rollout record: a ``turn_context`` entry whose
payload carries the effort the spawn named. The field name (``effort``,
plain, inside the ``turn_context`` payload) was confirmed 2026-09-11
against a real rollout on this machine before being asserted here.

The control spawn carries no ``--effort``; its rollout reads the configured
default (``model_reasoning_effort`` from ``~/.codex/config.toml``, codex's
own fallback ``medium`` when unset). That proves the reader, not the flag.

The test is opt-in because it spends real tokens and needs codex
credentials. It is never to be deleted: it is the only proof that the
spawn front door, the client argv, the daemon lane and the app-server all
agree on the effort axis end to end.
"""

from __future__ import annotations

import json
import os
import subprocess
import time
import tomllib
from pathlib import Path

import pytest

LIVE = os.environ.get("FNO_CODEX_LIVE") == "1"
_SKIP = "live codex journey spends tokens; set FNO_CODEX_LIVE=1 to run"

pytestmark = pytest.mark.skipif(not LIVE, reason=_SKIP)


def _configured_default_effort() -> str:
    cfg = Path.home() / ".codex" / "config.toml"
    try:
        data = tomllib.loads(cfg.read_text())
    except (OSError, tomllib.TOMLDecodeError):
        return "medium"
    return str(data.get("model_reasoning_effort") or "medium")


def _spawn(name: str, extra: list[str]) -> None:
    subprocess.run(
        ["fno", "agents", "spawn", "--name", name, "--harness", "codex",
         "--substrate", "thread", "Reply with the single word: ready.", *extra],
        check=True,
        capture_output=True,
        text=True,
        timeout=120,
    )


def _rollout_path(name: str, deadline_s: float = 30.0) -> Path:
    deadline = time.monotonic() + deadline_s
    while time.monotonic() < deadline:
        out = subprocess.run(
            ["fno", "agents", "list", "--json"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        for row in json.loads(out) if out.strip().startswith("[") else json.loads(out).get("sessions", []):
            if row.get("name") == name and row.get("log_path"):
                return Path(row["log_path"])
        time.sleep(0.5)
    raise AssertionError(f"no rollout recorded for {name} within {deadline_s}s")


def _rollout_efforts(rollout: Path) -> list[str]:
    efforts: list[str] = []
    for line in rollout.read_text().splitlines():
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("type") == "turn_context":
            effort = rec.get("payload", {}).get("effort")
            if effort:
                efforts.append(effort)
    return efforts


def _stop(name: str) -> None:
    subprocess.run(
        ["fno", "agents", "stop", name], capture_output=True, text=True, timeout=60
    )


def test_ac10_hp_a_real_codex_thread_carries_the_requested_effort(tmp_path) -> None:
    treatment = "x-c9ce-effort-t"
    control = "x-c9ce-effort-c"
    try:
        _spawn(treatment, ["--effort", "high"])
        _spawn(control, [])

        treatment_rollout = _rollout_path(treatment)
        control_rollout = _rollout_path(control)

        deadline = time.monotonic() + 30.0
        while time.monotonic() < deadline and not _rollout_efforts(treatment_rollout):
            time.sleep(0.5)
        efforts = _rollout_efforts(treatment_rollout)
        assert efforts, f"no turn_context ever landed in {treatment_rollout}"
        assert (
            efforts[0] == "high"
        ), f"the spawn named --effort high; the rollout reads {efforts!r}"

        control_efforts = _rollout_efforts(control_rollout)
        assert control_efforts, f"no turn_context ever landed in {control_rollout}"
        assert (
            control_efforts[0] == _configured_default_effort()
        ), f"the control must read the configured default, got {control_efforts!r}"
    finally:
        _stop(treatment)
        _stop(control)
