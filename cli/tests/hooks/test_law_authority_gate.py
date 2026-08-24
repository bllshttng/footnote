"""Fail-closed PreToolUse gate for the exact law enact command."""

from __future__ import annotations

import importlib.util
from pathlib import Path


REPO_ROOT = Path(__file__).parents[3]
GATE_PATH = REPO_ROOT / "hooks" / "law-authority-gate.py"
PROPOSAL_ID = "lp-0123456789ab"
CONTENT_HASH = "a" * 64
COMMAND = f"fno law enact --proposal {PROPOSAL_ID} --hash {CONTENT_HASH}"


def _gate():
    spec = importlib.util.spec_from_file_location("law_authority_gate", GATE_PATH)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _payload(command: str = COMMAND, **overrides: object) -> dict[str, object]:
    payload: dict[str, object] = {
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "session_id": "human-session-1",
        "permission_mode": "default",
        "cwd": str(REPO_ROOT),
    }
    payload.update(overrides)
    return payload


def test_exact_enact_command_asks_and_binds_preview_fields() -> None:
    gate = _gate()
    seen: list[dict[str, str]] = []

    def arm(**kwargs: str) -> dict[str, object]:
        seen.append(kwargs)
        return {
            "proposal_id": PROPOSAL_ID,
            "subject": "x-12ba",
            "decision": "Merges belong to the operator",
            "rationale": "Durable policy needs approval.",
            "options": ["operator", "agent"],
            "supersedes": None,
        }

    result = gate.evaluate(_payload(), arm=arm)

    assert result["hookSpecificOutput"]["permissionDecision"] == "ask"
    reason = result["hookSpecificOutput"]["permissionDecisionReason"]
    assert "x-12ba" in reason
    assert "Merges belong to the operator" in reason
    assert seen == [
        {
            "proposal_id": PROPOSAL_ID,
            "content_hash": CONTENT_HASH,
            "session_id": "human-session-1",
            "permission_mode": "default",
            "tool_input": COMMAND,
        }
    ]


def test_non_prompting_modes_deny_without_arming() -> None:
    gate = _gate()
    calls: list[dict[str, str]] = []

    for mode in ("bypassPermissions", "yolo", "dontAsk", "headless", "auto", "mystery"):
        result = gate.evaluate(_payload(permission_mode=mode), arm=lambda **kwargs: calls.append(kwargs))
        assert result["hookSpecificOutput"]["permissionDecision"] == "deny"
        assert "/fno:law resume <proposal-id>" in result["hookSpecificOutput"]["permissionDecisionReason"]

    assert calls == []


def test_malformed_or_wrapped_commands_deny() -> None:
    gate = _gate()
    commands = (
        f"bash -lc '{COMMAND}'",
        f"{COMMAND} && echo bypass",
        f"fno law enact --hash {CONTENT_HASH} --proposal {PROPOSAL_ID}",
        f"fno law enact --proposal {PROPOSAL_ID} --hash {'b' * 64}",
        f"fno law enact --proposal ../{PROPOSAL_ID} --hash {CONTENT_HASH}",
    )

    for command in commands:
        result = gate.evaluate(_payload(command), arm=lambda **kwargs: {})
        assert result["hookSpecificOutput"]["permissionDecision"] == "deny"

    missing_tool = gate.evaluate(
        _payload(tool_name=None),
        arm=lambda **kwargs: {},
    )
    assert missing_tool["hookSpecificOutput"]["permissionDecision"] == "deny"


def test_unrelated_bash_commands_are_not_decided() -> None:
    gate = _gate()

    assert gate.evaluate(_payload("git status"), arm=lambda **kwargs: {}) is None


def test_missing_session_and_arm_failure_deny() -> None:
    gate = _gate()
    missing = gate.evaluate(_payload(session_id=""), arm=lambda **kwargs: {})
    assert missing["hookSpecificOutput"]["permissionDecision"] == "deny"
    failed = gate.evaluate(_payload(), arm=lambda **kwargs: (_ for _ in ()).throw(RuntimeError("unreadable")))
    assert failed["hookSpecificOutput"]["permissionDecision"] == "deny"
    assert "unreadable" in failed["hookSpecificOutput"]["permissionDecisionReason"]
