#!/usr/bin/env python3
"""Ask for human approval before the exact staged-law enact command runs."""

from __future__ import annotations

import json
import os
import re
import shlex
import sys
from collections.abc import Callable
from pathlib import Path
from typing import Any


PROMPTING_MODES = frozenset({"default", "acceptEdits", "plan"})
PROPOSAL_ID = re.compile(r"^lp-[0-9a-f]{12}$")
CONTENT_HASH = re.compile(r"^[0-9a-f]{64}$")
RECOVERY = "/fno:law resume <proposal-id>"


def _output(
    decision: str,
    reason: str,
    *,
    updated_input: dict[str, Any] | None = None,
) -> dict[str, Any]:
    specific: dict[str, Any] = {
        "hookEventName": "PreToolUse",
        "permissionDecision": decision,
        "permissionDecisionReason": reason,
    }
    if updated_input is not None:
        specific["updatedInput"] = updated_input
    return {
        "hookSpecificOutput": specific
    }


def _deny(reason: str) -> dict[str, Any]:
    return _output("deny", f"fno inbox law refused: {reason}. Resume from an attended chat with {RECOVERY}.")


def _contains_enact_action(command: Any) -> bool:
    """Find enact only at a shell command position, not inside argument text."""
    if not isinstance(command, str) or not command.strip():
        return False
    lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|")
    lexer.whitespace_split = True
    segment: list[str] = []
    segments: list[list[str]] = []
    try:
        for token in lexer:
            if token in {";", "&&", "||", "|", "&"}:
                if segment:
                    segments.append(segment)
                segment = []
            else:
                segment.append(token)
    except ValueError:
        return bool(re.match(r"^\s*fno\s+inbox\s+law\s+enact\b", command))
    if segment:
        segments.append(segment)

    for tokens in segments:
        if tokens[:4] == ["fno", "inbox", "law", "enact"]:
            return True
        for index, token in enumerate(tokens[:-1]):
            if token in {"bash", "sh", "zsh"} and tokens[index + 1] in {"-c", "-lc"}:
                if _contains_enact_action(tokens[index + 2] if index + 2 < len(tokens) else ""):
                    return True
    return False


def _parse_command(command: Any) -> tuple[str, str] | None:
    if not isinstance(command, str) or not command.strip():
        return None
    try:
        tokens = shlex.split(command, posix=True)
    except ValueError:
        return None
    if len(tokens) != 8:
        return None
    if tokens[0:5] != ["fno", "inbox", "law", "enact", "--proposal"]:
        return None
    if tokens[6] != "--hash":
        return None
    proposal_id, content_hash = tokens[5], tokens[7]
    if not PROPOSAL_ID.fullmatch(proposal_id) or not CONTENT_HASH.fullmatch(content_hash):
        return None
    canonical = f"fno inbox law enact --proposal {proposal_id} --hash {content_hash}"
    if command != canonical:
        return None
    return proposal_id, content_hash


def _arm_from_current_source(**kwargs: str) -> dict[str, Any]:
    cwd = Path(kwargs["cwd"]).resolve()
    if not cwd.is_dir():
        raise RuntimeError("session cwd is not a directory")
    plugin_root = Path(__file__).resolve().parent.parent
    sys.path.insert(0, str(plugin_root / "cli" / "src"))
    from fno import law

    previous = Path.cwd()
    os.chdir(cwd)
    try:
        return law._arm_proposal_from_hook(
            kwargs["proposal_id"],
            content_hash=kwargs["content_hash"],
            session_id=kwargs["session_id"],
            permission_mode=kwargs["permission_mode"],
            tool_input=kwargs["tool_input"],
        )
    finally:
        os.chdir(previous)


def _preview(proposal: dict[str, Any]) -> str:
    options = ", ".join(str(value) for value in proposal.get("options") or []) or "none"
    return (
        f"Human approval required for law proposal {proposal.get('proposal_id')}: "
        f"subject={proposal.get('subject')!r}; decision={proposal.get('decision')!r}; "
        f"rationale={proposal.get('rationale')!r}; options={options!r}; "
        f"supersedes={proposal.get('supersedes')!r}."
    )


def evaluate(
    payload: dict[str, Any],
    *,
    arm: Callable[..., dict[str, Any]] = _arm_from_current_source,
) -> dict[str, Any] | None:
    """Return a permission decision only for the canonical enact command."""
    tool_name = payload.get("tool_name")
    tool_input = payload.get("tool_input")
    command = tool_input.get("command") if isinstance(tool_input, dict) else None
    if not _contains_enact_action(command):
        return None
    if tool_name != "Bash":
        return _deny("tool name is missing") if tool_name is None else None
    if not isinstance(tool_input, dict):
        return _deny("tool input is unreadable")
    parsed = _parse_command(command)
    if parsed is None:
        return _deny("command is not the exact canonical enact action")
    proposal_id, content_hash = parsed
    mode = payload.get("permission_mode")
    if mode not in PROMPTING_MODES:
        return _deny(f"permission mode {mode!r} cannot provide human approval")
    session_id = payload.get("session_id")
    if not isinstance(session_id, str) or not session_id.strip():
        return _deny("session id is missing")
    try:
        proposal = arm(
            proposal_id=proposal_id,
            content_hash=content_hash,
            session_id=session_id,
            permission_mode=mode,
            tool_input=command,
            cwd=str(payload.get("cwd") or ""),
        )
    except Exception as exc:  # noqa: BLE001 - a hook failure must deny this action
        return _deny(f"proposal arm failed: {exc}")
    if proposal.get("proposal_id") != proposal_id:
        return _deny("arm receipt does not name the requested proposal")
    receipt = proposal.get("approval_receipt")
    updated_tool_input = proposal.get("armed_tool_input")
    if not isinstance(receipt, str) or not isinstance(updated_tool_input, str):
        return _deny("arm receipt does not include an approval token")
    if updated_tool_input != f"{command} --receipt {receipt}":
        return _deny("arm receipt does not bind the approval token to this command")
    return _output(
        "ask",
        _preview(proposal),
        updated_input={"command": updated_tool_input},
    )


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except (json.JSONDecodeError, OSError):
        print(json.dumps(_deny("hook payload is unreadable")))
        return 0
    result = evaluate(payload if isinstance(payload, dict) else {})
    if result is not None:
        print(json.dumps(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
