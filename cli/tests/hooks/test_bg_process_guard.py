"""The unbounded-background-process guard: what it refuses and what it must not.

Both halves matter. A guard that denies too little is decorative; a guard that
denies a legitimate command is a guard someone disables.
"""
from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[3]
HOOK = ROOT / "hooks" / "bg-process-guard.py"


def _load():
    spec = importlib.util.spec_from_file_location("bg_process_guard", HOOK)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


guard = _load()


# The refusal cases: each can run forever and none carries a bound.
DENIED = [
    "yes > /dev/null &",
    "yes > /dev/null",
    "yes",
    "nohup yes > /dev/null 2>&1 &",
    "cd /tmp && yes > /dev/null &",
    "bash -c 'yes > /dev/null' &",
    "while true; do echo hi; done",
    "while :; do sleep 1; done",
    "until false; do echo hi; done",
    "sleep infinity",
    "stress -c 8",
    "dd if=/dev/zero of=/dev/null",
    "cat /dev/urandom > /dev/null",
    "setsid yes > /dev/null &",
]

# The allow cases. Two kinds: bounded versions of the same generators, and
# ordinary commands that merely mention a generator word.
ALLOWED = [
    "timeout 300 bash -c 'exec -a fno-load-x1 yes > /dev/null'",
    "timeout 300 yes > /dev/null",
    "gtimeout 60 stress -c 8",
    "yes | head -c 1M > /dev/null",
    "dd if=/dev/zero of=/dev/null count=1000",
    "stress -c 8 -t 60",
    "echo yes",
    "git commit -m 'yes it works'",
    "grep -rn --include=*.py -e nh_operator_members_facil .",
    "npm test && echo yes",
    "rg yes .",
    "printf 'yes\\n' | wc -l",
    # The repo's own bounded detached work. Every stage inside is wrapped by
    # with_timeout, so this must keep working; a tightening that blocks it
    # breaks the eval sweep on every session start.
    "nohup bash -c 'source \"$1/eval-sweep-throttle.sh\" || exit 0; "
    "_eval_sweep_run_stages \"$2\" \"$3\" \"$4\"' _ a b c d >/dev/null 2>&1 &",
    "gh pr checks --watch",
    "cargo test",
]


@pytest.mark.parametrize("command", DENIED)
def test_denies_unbounded_generators(command: str) -> None:
    assert guard.decide(command) is not None, f"should have refused: {command}"


@pytest.mark.parametrize("command", ALLOWED)
def test_allows_bounded_and_ordinary_commands(command: str) -> None:
    assert guard.decide(command) is None, f"should have allowed: {command}"


def test_refusal_carries_the_replacement_verbatim() -> None:
    """The refusal string IS the naming layer. Prevention layer 4 ships here
    and nowhere else, so a reason without the bounded+named form is a silent
    regression of a whole layer of the design."""
    reason = guard.decide("yes > /dev/null &")
    assert reason is not None
    assert "timeout" in reason
    assert "exec -a fno-" in reason
    assert "SIGPIPE" in reason


def test_unbalanced_quotes_fail_open() -> None:
    assert guard.decide("echo \"oops\nyes > /dev/null &") is None


def _run_hook(payload: object) -> tuple[int, str]:
    proc = subprocess.run(
        [sys.executable, str(HOOK)],
        input=json.dumps(payload),
        capture_output=True,
        text=True,
    )
    return proc.returncode, proc.stdout


def test_end_to_end_deny_envelope() -> None:
    code, out = _run_hook(
        {"tool_name": "Bash", "tool_input": {"command": "yes > /dev/null &"}}
    )
    assert code == 0
    decision = json.loads(out)["hookSpecificOutput"]
    assert decision["permissionDecision"] == "deny"
    assert "exec -a fno-" in decision["permissionDecisionReason"]


def test_end_to_end_allow_is_silent() -> None:
    code, out = _run_hook(
        {"tool_name": "Bash", "tool_input": {"command": "echo yes"}}
    )
    assert code == 0
    assert out.strip() == ""


def test_non_bash_tool_is_ignored() -> None:
    code, out = _run_hook(
        {"tool_name": "Write", "tool_input": {"command": "yes > /dev/null &"}}
    )
    assert code == 0
    assert out.strip() == ""


def test_malformed_stdin_allows() -> None:
    proc = subprocess.run(
        [sys.executable, str(HOOK)], input="not json", capture_output=True, text=True
    )
    assert proc.returncode == 0
    assert proc.stdout.strip() == ""
