"""Inbox triage seam: LLM-backed decision on heads-up threads.

Shells out to ``claude -p`` (or an ``FNO_INBOX_TRIAGE_STUB`` test script).
Refuses to call the real LLM in pytest or CI when no stub is configured.

Public API:
    TriageSettings      - configuration dataclass
    TriagePlan          - result dataclass
    TriageFailedError   - raised when triage subprocess fails twice
    read_triage_settings - read config from nearest settings.yaml
    triage_thread       - run LLM triage on a ThreadHandle (post-2026-05)
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Literal, Optional

import yaml

from fno import _subprocess_util
from fno.inbox.store import (
    ThreadHandle,
    resolve_project,
)


# ---------------------------------------------------------------------------
# Schema passed to claude -p
# ---------------------------------------------------------------------------

SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ["create_node", "ignore", "request_clarification"],
        },
        "title": {"type": ["string", "null"]},
        "priority": {
            "type": ["string", "null"],
            "enum": ["p0", "p1", "p2", "p3", None],
        },
        "body": {"type": "string"},
        "follow_up_question": {"type": ["string", "null"]},
    },
    "required": ["action", "body"],
    "additionalProperties": False,
}

_VALID_PRIORITIES = {"p0", "p1", "p2", "p3"}
_VALID_ACTIONS = {"create_node", "ignore", "request_clarification"}


# ---------------------------------------------------------------------------
# Dataclasses
# ---------------------------------------------------------------------------

@dataclass
class TriageSettings:
    model: Optional[str] = None
    timeout_sec: int = 60
    log_decisions: bool = True


@dataclass
class TriagePlan:
    action: Literal["create_node", "ignore", "request_clarification"]
    title: Optional[str]
    priority: Optional[str]
    body: str
    follow_up_question: Optional[str]


class TriageFailedError(Exception):
    """Raised when triage subprocess fails twice or returns invalid output."""


# ---------------------------------------------------------------------------
# Settings reader
# ---------------------------------------------------------------------------

def read_triage_settings(cwd: Optional[Path] = None) -> TriageSettings:
    """Read TriageSettings from nearest .fno/ config (config.toml, else legacy
    settings.yaml)."""
    from fno.config import read_config_flat

    search = cwd if cwd is not None else Path.cwd()

    while True:
        fno_dir = search / ".fno"
        toml_file = fno_dir / "config.toml"
        yaml_file = fno_dir / "settings.yaml"
        data: Optional[dict] = None
        if toml_file.is_file():
            data = read_config_flat(toml_file)
        elif yaml_file.is_file():
            data = _read_triage_yaml(yaml_file)
        if isinstance(data, dict):
            inbox = data.get("inbox")
            triage_cfg = (inbox.get("triage") if isinstance(inbox, dict) else None) or {}
            try:
                timeout_sec = int(triage_cfg.get("timeout_sec", 60))
            except (ValueError, TypeError):
                # A malformed timeout_sec must fail safe to the default, not
                # crash the reader (gemini review).
                timeout_sec = 60
            return TriageSettings(
                model=triage_cfg.get("model", None),
                timeout_sec=timeout_sec,
                log_decisions=bool(triage_cfg.get("log_decisions", True)),
            )

        parent = search.parent
        if parent == search:
            break
        search = parent

    return TriageSettings()


def _read_triage_yaml(path: Path) -> dict:
    """Parse a legacy settings.yaml to the flat (config-unwrapped) shape, warning
    to stderr on an unreadable/malformed file so a config typo stays observable."""
    try:
        data = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    except (OSError, yaml.YAMLError) as e:
        print(
            f"warning: malformed {path}: {type(e).__name__}: {e}",
            file=sys.stderr,
        )
        return {}
    config = data.get("config") if isinstance(data, dict) else None
    return config if isinstance(config, dict) else {}


# ---------------------------------------------------------------------------
# Prompt builder
# ---------------------------------------------------------------------------

def _build_prompt(handle: ThreadHandle, receiver: str, cwd: Path) -> str:
    """Build the triage prompt for the LLM. Includes full thread context."""
    claudemd_path = cwd / "CLAUDE.md"
    if claudemd_path.is_file():
        try:
            cwd_claudemd_excerpt = claudemd_path.read_text(encoding="utf-8")[:1000]
        except OSError:
            cwd_claudemd_excerpt = "[no CLAUDE.md]"
    else:
        cwd_claudemd_excerpt = "[no CLAUDE.md]"

    backlog_summary = ""
    backlog_failed = False
    backlog_failure_reason = ""
    try:
        result = subprocess.run(
            [*_subprocess_util.fno_py_cmd(), "backlog", "ready", "--project", receiver, "--json"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode == 0 and result.stdout.strip():
            try:
                entries = json.loads(result.stdout)
                if isinstance(entries, list):
                    entries = entries[:10]
                    backlog_summary = json.dumps(entries, indent=2)
            except (json.JSONDecodeError, TypeError) as e:
                backlog_failed = True
                backlog_failure_reason = f"json decode: {e}"
        elif result.returncode != 0:
            backlog_failed = True
            backlog_failure_reason = (
                f"fno backlog ready exit {result.returncode}: "
                f"{(result.stderr or '').strip()[:200]}"
            )
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as e:
        backlog_failed = True
        backlog_failure_reason = f"{type(e).__name__}: {e}"

    if backlog_failed:
        print(
            f"warning: backlog summary fetch failed for {receiver}: "
            f"{backlog_failure_reason}",
            file=sys.stderr,
        )

    refs_str = json.dumps(handle.refs) if handle.refs else "{}"

    thread_text_parts: list[str] = []
    for m in handle.messages:
        thread_text_parts.append(
            f"--- msg-block ---\n"
            f"msg_id: {m.msg_id}\n"
            f"from: {m.from_project}\n"
            f"timestamp: {m.timestamp.isoformat()}\n"
            f"body:\n{m.body}\n"
        )
    thread_text = "\n".join(thread_text_parts)

    prompt = (
        f'You are a triage agent reading a thread addressed to project "{receiver}".\n\n'
        f"Thread kind: {handle.kind}\n"
        f"Refs: {refs_str}\n"
        f"Original sender: {handle.from_project}\n\n"
        f"Thread messages (root + replies, oldest first):\n{thread_text}\n\n"
        f"Receiver context (project CLAUDE.md if present):\n{cwd_claudemd_excerpt}\n\n"
        f"Receiver open backlog (top 10):\n"
        f"{backlog_summary if backlog_summary else ('[backlog fetch failed: ' + backlog_failure_reason + ']' if backlog_failed else '[no backlog]')}\n\n"
        "Decide:\n"
        "- create_node: file a new graph entry. Provide title + priority.\n"
        "- ignore: not actionable for this project. Provide brief body explaining why.\n"
        "- request_clarification: need more info. Provide follow_up_question.\n\n"
        "Respond with the JSON schema. No prose outside JSON."
    )
    return prompt


# ---------------------------------------------------------------------------
# Parse and validate
# ---------------------------------------------------------------------------

def _parse_and_validate(output: str) -> TriagePlan:
    data = json.loads(output)

    if not isinstance(data, dict):
        raise ValueError("schema violation: response is not a JSON object")

    action = data.get("action")
    if action not in _VALID_ACTIONS:
        raise ValueError(
            f"schema violation: invalid action {action!r}, "
            f"expected one of {sorted(_VALID_ACTIONS)}"
        )

    body = data.get("body")
    if not isinstance(body, str) or not body:
        raise ValueError("schema violation: 'body' is required and must be a non-empty string")

    title = data.get("title")
    priority = data.get("priority")
    follow_up_question = data.get("follow_up_question")

    if action == "create_node":
        if not title:
            raise ValueError("schema violation: 'title' required when action=create_node")
        if priority not in _VALID_PRIORITIES:
            raise ValueError(
                f"schema violation: 'priority' must be one of {sorted(_VALID_PRIORITIES)} "
                f"when action=create_node, got {priority!r}"
            )

    if action == "request_clarification":
        if not follow_up_question:
            raise ValueError(
                "schema violation: 'follow_up_question' required when action=request_clarification"
            )

    return TriagePlan(
        action=action,
        title=title if title else None,
        priority=priority if priority else None,
        body=body,
        follow_up_question=follow_up_question if follow_up_question else None,
    )


# ---------------------------------------------------------------------------
# Logs
# ---------------------------------------------------------------------------

def _log_decision(plan: TriagePlan, thread_id: str, log_path: Path) -> None:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    entry = {
        "ts": datetime.now(timezone.utc).isoformat(),
        "thread_id": thread_id,
        "action": plan.action,
        "title": plan.title,
    }
    with log_path.open("a", encoding="utf-8") as f:
        f.write(json.dumps(entry) + "\n")


def _log_triage_error(thread_id: str, reason: str, errors_path: Path) -> None:
    errors_path.parent.mkdir(parents=True, exist_ok=True)
    entry = {
        "ts": datetime.now(timezone.utc).isoformat(),
        "thread_id": thread_id,
        "reason": reason,
    }
    with errors_path.open("a", encoding="utf-8") as f:
        f.write(json.dumps(entry) + "\n")


# ---------------------------------------------------------------------------
# Subprocess runner
# ---------------------------------------------------------------------------

def _build_claude_cmd(schema: dict, settings: TriageSettings) -> list[str]:
    """Build the real ``claude -p`` argv for a triage call.

    ``--bare`` skips Claude Code's normal auth precedence: per the authentication
    docs it reads ONLY ``ANTHROPIC_API_KEY`` or an ``apiKeyHelper`` - never the
    keychain OAuth credential or ``CLAUDE_CODE_OAUTH_TOKEN``. On a
    subscription-OAuth machine with no API key, ``--bare`` returns
    "Not logged in" and silently strands the drain. (Inbox triage shipped with
    ``--bare`` hardcoded, so it never worked under subscription auth.) Pass
    ``--bare`` ONLY when an API key is present - the one mode where it can
    authenticate - and otherwise use plain ``claude -p``, which honors the full
    auth precedence (OAuth included). ``--bare`` only saves a little startup
    time; it is not worth losing auth for a triage call.
    """
    cmd = ["claude", "-p"]
    if os.environ.get("ANTHROPIC_API_KEY"):
        cmd.append("--bare")
    cmd += [
        "--output-format", "json",
        "--json-schema", json.dumps(schema),
        "--append-system-prompt", "You are a triage agent. Respond with JSON only.",
    ]
    if settings.model:
        cmd.extend(["--model", settings.model])
    return cmd


def _raise_on_claude_error(stdout: str) -> None:
    """Fail loud when claude's JSON envelope reports an error.

    ``claude -p`` exits 0 even on auth/runtime failure, signalling it only in the
    envelope (``{"is_error": true, "result": "Not logged in ..."}``). ``check=True``
    is exit-code-only and misses this, so the envelope would otherwise flow into
    ``_parse_and_validate`` and surface as a confusing generic "schema violation"
    instead of the real cause. A successful schema response is the schema object
    itself (no ``is_error`` key), so this never fires on success; non-JSON output
    (e.g. a test stub) is left for normal parsing. Raises ``ValueError`` so the
    existing retry/``TriageFailedError`` path logs the real reason.
    """
    try:
        data = json.loads(stdout)
    except (json.JSONDecodeError, ValueError):
        return
    if isinstance(data, dict) and data.get("is_error"):
        detail = data.get("result") or data.get("error") or "unknown error"
        hint = ""
        if isinstance(detail, str) and "logged in" in detail.lower():
            hint = (
                " (auth: `--bare` needs ANTHROPIC_API_KEY; without it plain "
                "`claude -p` uses your subscription OAuth)"
            )
        raise ValueError(f"claude -p returned is_error: {detail}{hint}")


def _run_claude_p(prompt: str, schema: dict, settings: TriageSettings) -> str:
    """Run claude -p (or stub) with prompt on stdin. Return stdout string."""
    stub_path = os.environ.get("FNO_INBOX_TRIAGE_STUB")

    in_pytest = os.environ.get("PYTEST_CURRENT_TEST") is not None
    in_ci = os.environ.get("CI", "").lower() in ("true", "1", "yes")
    if not stub_path and (in_pytest or in_ci):
        raise RuntimeError(
            "FNO_INBOX_TRIAGE_STUB not configured; "
            "refusing to call real claude -p in tests"
        )

    if stub_path:
        cmd = [stub_path]
    else:
        cmd = _build_claude_cmd(schema, settings)

    result = subprocess.run(
        cmd,
        input=prompt,
        capture_output=True,
        text=True,
        timeout=settings.timeout_sec,
        check=True,
    )
    _raise_on_claude_error(result.stdout)
    return result.stdout


# ---------------------------------------------------------------------------
# Main triage function (thread-aware)
# ---------------------------------------------------------------------------

def triage_thread(
    handle: ThreadHandle,
    settings: Optional[TriageSettings] = None,
    cwd: Optional[Path] = None,
    project_override: Optional[str] = None,
) -> TriagePlan:
    """Run LLM triage on a thread. The full message history is included in the prompt."""
    if settings is None:
        settings = read_triage_settings(cwd=cwd)

    from fno.paths import project_log

    effective_cwd = cwd if cwd is not None else Path.cwd()
    receiver = resolve_project(cwd=effective_cwd, override=project_override)

    prompt = _build_prompt(handle, receiver, effective_cwd)
    errors_path = project_log("inbox-errors.jsonl")
    log_path = project_log("triage-log.jsonl")

    last_error: Optional[Exception] = None

    for attempt in range(2):
        attempt_prompt = prompt
        if attempt == 1:
            attempt_prompt = (
                prompt
                + "\n\nSTRICT REMINDER: respond with ONLY a valid JSON object. "
                "No prose, no markdown, no code fences. "
                "Required fields: action (one of: create_node, ignore, request_clarification), "
                "body (string). "
                "For create_node: title (string) and priority (p0/p1/p2/p3) are REQUIRED. "
                "For request_clarification: follow_up_question (string) is REQUIRED."
            )

        try:
            raw = _run_claude_p(attempt_prompt, SCHEMA, settings)
            plan = _parse_and_validate(raw)
            if settings.log_decisions:
                _log_decision(plan, handle.thread_id, log_path)
            return plan
        except (
            json.JSONDecodeError,
            ValueError,
            subprocess.CalledProcessError,
            subprocess.TimeoutExpired,
        ) as exc:
            last_error = exc
            attempt_label = "attempt 1" if attempt == 0 else "attempt 2 (retry)"
            print(
                f"warning: triage {attempt_label} failed for {handle.thread_id}: "
                f"{type(exc).__name__}: {exc}",
                file=sys.stderr,
            )

    reason = f"{type(last_error).__name__}: {last_error}"
    _log_triage_error(handle.thread_id, reason, errors_path)
    raise TriageFailedError(
        f"Triage failed twice for {handle.thread_id}: {reason}"
    ) from last_error
