"""Contract test: every Codex Stop handler prints one JSON object or nothing.

Codex 0.154.0 parses a successful (exit 0) synchronous Stop handler's
stdout as ONE strict Stop JSON object; plain text fails the hook with
"hook returned invalid stop hook JSON output" and a finished worker never
continues; two workers sat idle, 2026-09-10 and 2026-09-13.
Exit 2 with non-empty stderr is a block; any other exit fails.

Mirrors rust-v0.154.0 codex-rs/hooks/src/events/stop.rs:277-341 (exit
handling), schema.rs:87-99 and 451-464 (StopCommandOutputWire: the
camelCase fields plus decision:"block" with a reason), and
engine/output_parser.rs (strict single-object parse). Codex's own
regression test events/stop.rs:623-649 pins (0, "not json", "") -> Failed.

The command list is read from hooks/codex-hooks.json at test time, never
hardcoded, so a new Stop registration is covered on arrival.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).parents[3]
CODEX_HOOKS_JSON = REPO_ROOT / "hooks" / "codex-hooks.json"
NUDGE = REPO_ROOT / "hooks" / "operator-capture-nudge.sh"

CODEX_INVALID_JSON = "hook returned invalid stop hook JSON output"

# Universal output fields + Stop's decision/reason (schema.rs 87-99, 451-464).
_STOP_ALLOWED_KEYS = {
    "continue",
    "stopReason",
    "suppressOutput",
    "systemMessage",
    "decision",
    "reason",
}

# serde types for the known fields (schema.rs 87-99); decision is enum-checked below.
_STOP_VALUE_TYPES = {
    "continue": bool,
    "stopReason": str,
    "suppressOutput": bool,
    "systemMessage": str,
    "reason": str,
}


def codex_stop_accepts(rc: int, stdout: str, stderr: str) -> str | None:
    """None = Codex 0.154.0 accepts the handler result, else the failure text.

    Exit 0 with empty (after strip) stdout is a clean no-op. Exit 0 with
    stdout must parse as one JSON object carrying only known keys, where
    decision is absent or "block" with a non-empty reason. Exit 2 needs
    non-empty stderr (the block). Anything else fails the hook.
    """
    if rc == 0:
        text = stdout.strip()
        if not text:
            return None
        try:
            parsed = json.loads(text)
        except json.JSONDecodeError:
            return CODEX_INVALID_JSON
        if not isinstance(parsed, dict):
            return CODEX_INVALID_JSON
        unknown = sorted(set(parsed) - _STOP_ALLOWED_KEYS)
        if unknown:
            return f"hook output has unknown Stop field(s): {unknown}"
        for key, value in parsed.items():
            expected = _STOP_VALUE_TYPES.get(key)
            if expected is not None and not isinstance(value, expected):
                return f"hook output field {key!r} must be {expected.__name__}"
        decision = parsed.get("decision")
        if decision is not None:
            if decision != "block":
                return f"hook output decision must be 'block', got {decision!r}"
            if not parsed.get("reason"):
                return "hook output decision 'block' requires a non-empty reason"
        return None
    if rc == 2:
        if stderr.strip():
            return None
        return "hook blocked (exit 2) with empty stderr"
    return f"hook exited {rc}"


def _fno_stub(bin_dir: Path, name: str, *, exit_code: int) -> Path:
    """A stub binary on PATH, same shape as test_operator_capture_nudge's."""
    stub = bin_dir / name
    stub.write_text(f"#!/usr/bin/env bash\nexit {exit_code}\n")
    stub.chmod(0o755)
    return stub


def _hook_env(tmp_path: Path, bin_dir: Path) -> dict[str, str]:
    """Isolated HOME/FNO_HOME/CODEX_HOME so no hook touches real state."""
    for sub in ("home", "fno-home", "codex-home"):
        (tmp_path / sub).mkdir(exist_ok=True)
    env = os.environ.copy()
    env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"
    env["HOME"] = str(tmp_path / "home")
    env["FNO_HOME"] = str(tmp_path / "fno-home")
    env["CODEX_HOME"] = str(tmp_path / "codex-home")
    for key in (
        "FNO_OPERATOR_SESSION_ID",
        "FNO_OPERATOR_TRANSCRIPT",
        "FNO_OPERATOR_CAPTURE_DIR",
    ):
        env.pop(key, None)
    return env


def _stop_payload(tmp_path: Path) -> str:
    """A minimal Codex Stop payload over a throwaway one-commit git repo."""
    repo = tmp_path / "work"
    repo.mkdir(exist_ok=True)
    identity = ["-c", "user.email=contract@test", "-c", "user.name=contract"]
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True, capture_output=True)
    (repo / "file.txt").write_text("x\n", encoding="utf-8")
    for args in (["add", "-A"], ["commit", "-qm", "init"]):
        subprocess.run(["git", *identity, *args], cwd=repo, check=True, capture_output=True)
    transcript = tmp_path / "transcript.jsonl"
    transcript.touch(exist_ok=True)
    return json.dumps(
        {
            "hook_event_name": "Stop",
            "session_id": "contract-test",
            "turn_id": "turn-1",
            "cwd": str(repo),
            "transcript_path": str(transcript),
            "stop_hook_active": False,
            "last_assistant_message": "",
        }
    )


def _stop_commands() -> list[str]:
    data = json.loads(CODEX_HOOKS_JSON.read_text(encoding="utf-8"))
    commands = [
        hook["command"]
        for registration in data["hooks"]["Stop"]
        for hook in registration.get("hooks", [])
    ]
    assert commands, "codex-hooks.json registers no Stop handlers"
    return [command.replace("${PLUGIN_ROOT}", str(REPO_ROOT)) for command in commands]


@pytest.fixture(params=[1, 0], ids=["fno-exit-1", "fno-exit-0"])
def stub_run(request, tmp_path):
    """(env, payload, runner) with stub fno/fno-agents under one exit code."""
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    for name in ("fno", "fno-agents"):
        _fno_stub(bin_dir, name, exit_code=request.param)
    env = _hook_env(tmp_path, bin_dir)
    payload = _stop_payload(tmp_path)

    def run(command: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", "-c", command],
            input=payload,
            env=env,
            cwd=str(tmp_path),
            capture_output=True,
            text=True,
            timeout=60,
        )

    return run


def test_stop_group_output_is_codex_valid(stub_run) -> None:
    """AC3-HP: every Stop command's (rc, stdout, stderr) passes the validator."""
    run = stub_run
    for command in _stop_commands():
        result = run(command)
        verdict = codex_stop_accepts(result.returncode, result.stdout, result.stderr)
        assert verdict is None, (
            f"{command}: rc={result.returncode} {verdict}\n"
            f"stdout={result.stdout!r}\nstderr={result.stderr[-400:]!r}"
        )


def test_nudge_markdown_is_rejected(tmp_path) -> None:
    """AC3-ERR positive control: the output that idled two workers fails the
    validator. Without this, a validator that accepts everything reads green."""
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    for name in ("fno", "fno-agents"):
        _fno_stub(bin_dir, name, exit_code=1)
    env = _hook_env(tmp_path, bin_dir)
    result = subprocess.run(
        ["bash", str(NUDGE)],
        input="",
        env=env,
        cwd=str(tmp_path),
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0
    assert result.stdout.strip(), "expected the failed-read Markdown branch"
    assert (
        codex_stop_accepts(result.returncode, result.stdout, result.stderr)
        == CODEX_INVALID_JSON
    )


def test_codex_regression_not_json_names_the_ui_error() -> None:
    """Codex's own case: (0, "not json", "") fails with the exact UI text."""
    assert codex_stop_accepts(0, "not json", "") == CODEX_INVALID_JSON


@pytest.mark.parametrize(
    ("result", "accepted"),
    [
        ((0, "", ""), True),
        ((0, "  \n\t", ""), True),
        ((0, '{"decision":"block","reason":"keep working"}', ""), True),
        ((0, '{"continue":false,"stopReason":"pr green"}', ""), True),
        ((0, '{"suppressOutput":true,"systemMessage":"note"}', ""), True),
        ((2, "", "blocked: findings outstanding\n"), True),
        ((0, "not json", ""), False),
        ((0, "[]", ""), False),
        ((0, '{"decision":"continue","reason":"x"}', ""), False),
        ((0, '{"decision":"block","reason":""}', ""), False),
        ((0, '{"decision":"block"}', ""), False),
        ((0, '{"surprise":true}', ""), False),
        ((0, '{"continue":"yes"}', ""), False),
        ((0, '{"stopReason":7}', ""), False),
        ((2, "", ""), False),
        ((1, '{"decision":"block","reason":"r"}', ""), False),
    ],
)
def test_validator_matches_codex_0_154_0(result: tuple, accepted: bool) -> None:
    verdict = codex_stop_accepts(*result)
    assert (verdict is None) is accepted, verdict
