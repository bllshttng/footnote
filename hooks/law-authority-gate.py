#!/usr/bin/env python3
"""Ask for human approval before the exact staged-law enact command runs."""

from __future__ import annotations

import json
import hashlib
import hmac
import os
import re
import secrets
import shlex
import subprocess
import sys
from collections.abc import Callable
from pathlib import Path
from typing import Any


PROMPTING_MODES = frozenset({"default", "manual", "plan"})
PROPOSAL_ID = re.compile(r"^lp-[0-9a-f]{12}$")
CONTENT_HASH = re.compile(r"^[0-9a-f]{64}$")
RECOVERY = "/fno:law resume <proposal-id>"
HOOK_SECRET = ".hook-secret"


def _output(decision: str, reason: str) -> dict[str, Any]:
    return {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    }


def _deny(reason: str) -> dict[str, Any]:
    return _output("deny", f"fno law refused: {reason}. Resume from an attended chat with {RECOVERY}.")


def _mentions_enact(command: Any) -> bool:
    return isinstance(command, str) and re.search(r"\bfno\s+law\s+enact\b", command) is not None


def _hook_proof(proposal_id: str, content_hash: str, session_id: str, mode: str, tool_input: str) -> str:
    directory = Path.cwd() / ".fno" / "law-proposals"
    directory.mkdir(parents=True, exist_ok=True)
    secret_path = directory / HOOK_SECRET
    try:
        secret = secret_path.read_bytes()
    except FileNotFoundError:
        secret = secrets.token_bytes(32)
        try:
            fd = os.open(secret_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        except FileExistsError:
            secret = secret_path.read_bytes()
        else:
            with os.fdopen(fd, "wb") as handle:
                handle.write(secret)
    message = "\0".join((proposal_id, content_hash, session_id, mode, tool_input))
    return hmac.new(secret, message.encode("utf-8"), hashlib.sha256).hexdigest()


def _parse_command(command: Any) -> tuple[str, str] | None:
    if not isinstance(command, str) or not command.strip():
        return None
    try:
        tokens = shlex.split(command, posix=True)
    except ValueError:
        return None
    if len(tokens) != 7:
        return None
    if tokens[0:4] != ["fno", "law", "enact", "--proposal"]:
        return None
    if tokens[5] != "--hash":
        return None
    proposal_id, content_hash = tokens[4], tokens[6]
    if not PROPOSAL_ID.fullmatch(proposal_id) or not CONTENT_HASH.fullmatch(content_hash):
        return None
    canonical = f"fno law enact --proposal {proposal_id} --hash {content_hash}"
    if command != canonical:
        return None
    return proposal_id, content_hash


def _arm_with_cli(**kwargs: str) -> dict[str, Any]:
    binary = os.environ.get("FNO_BIN", "fno")
    proc = subprocess.run(
        [
            binary,
            "law",
            "arm",
            "--proposal",
            kwargs["proposal_id"],
            "--hash",
            kwargs["content_hash"],
            "--session-id",
            kwargs["session_id"],
            "--permission-mode",
            kwargs["permission_mode"],
            "--tool-input",
            kwargs["tool_input"],
            "--proof",
            _hook_proof(
                kwargs["proposal_id"],
                kwargs["content_hash"],
                kwargs["session_id"],
                kwargs["permission_mode"],
                kwargs["tool_input"],
            ),
        ],
        cwd=kwargs.get("cwd") or None,
        capture_output=True,
        text=True,
        check=False,
        timeout=5,
    )
    if proc.returncode != 0:
        raise RuntimeError(proc.stderr.strip() or "arm command failed")
    value = json.loads(proc.stdout)
    if not isinstance(value, dict):
        raise RuntimeError("arm command returned an invalid receipt")
    return value


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
    arm: Callable[..., dict[str, Any]] = _arm_with_cli,
) -> dict[str, Any] | None:
    """Return a permission decision only for the canonical enact command."""
    tool_name = payload.get("tool_name")
    tool_input = payload.get("tool_input")
    command = tool_input.get("command") if isinstance(tool_input, dict) else None
    if not _mentions_enact(command):
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
        )
    except Exception as exc:  # noqa: BLE001 - a hook failure must deny this action
        return _deny(f"proposal arm failed: {exc}")
    if proposal.get("proposal_id") != proposal_id:
        return _deny("arm receipt does not name the requested proposal")
    return _output("ask", _preview(proposal))


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
