#!/usr/bin/env python3
"""Run or verify the isolated Codex Stop/goal continuation proof.

The writer owns only private roots.  It never sends mail, writes to the live
Codex home, or uses the live FNO graph.  A receipt is written only after the
positive evidence has been observed; the verifier treats missing evidence as
a named failure rather than guessing from a global status row.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
import uuid
from pathlib import Path
from typing import Any


DEFAULT_ROOT = Path("/private/tmp/fno-continuation-proof")
RECEIPT_PREFIX = "codex_reign_continuation_"
RECEIPT_SCHEMA_VERSION = 3
FAILURE_CLASSES = {
    "plugin-missing",
    "machine-installed-session-refresh-unverified",
    "hooks-disabled",
    "session-hook-unobserved",
    "identity-miss",
    "malformed-output",
    "hook-timeout",
    "parser-rejected",
    "explicit-park",
    "wake-disabled",
    "wake-budget-spent",
    "wake-refused",
    "compaction-marker-stale",
    "resume-marker-stale",
}
REQUIRED_FIELDS = (
    "schema_version",
    "created_at",
    "versions",
    "session",
    "correlation_id",
    "continuation_owner",
    "action_hash",
    "user_message_count",
    "command_requests",
    "window",
    "goal",
    "stop",
    "proof",
    "status",
)
FULL_SESSION_ID = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")


def _failure(name: str, reader: str) -> dict[str, Any]:
    return {"ok": False, "class": name, "failed_reader": reader}


def _nested(data: dict[str, Any], *keys: str) -> Any:
    current: Any = data
    for key in keys:
        if not isinstance(current, dict) or key not in current:
            return None
        current = current[key]
    return current


def _require_provider_receipt(
    receipt: Any, *, session_id: str, action: str
) -> dict[str, Any]:
    """Accept only a verified Codex action receipt for the exact thread."""
    if not isinstance(receipt, dict):
        raise RuntimeError("malformed-output: provider receipt is not an object")
    if receipt.get("thread_id") != session_id:
        raise RuntimeError("identity-miss: provider receipt thread id")
    if (
        receipt.get("verified") is not True
        or receipt.get("provider") != "codex"
        or receipt.get("action") != action
    ):
        raise RuntimeError("parser-rejected: provider action receipt is not verified")
    return receipt


def classify_receipt(receipt: dict[str, Any]) -> dict[str, Any]:
    """Return one positive verdict or exactly one named failed reader."""
    if receipt.get("schema_version") != RECEIPT_SCHEMA_VERSION:
        return _failure("malformed-output", "receipt.schema_version")
    failure = receipt.get("failure")
    if isinstance(failure, dict) and failure.get("class") in FAILURE_CLASSES:
        reader = failure.get("reader")
        if isinstance(reader, str) and reader:
            return _failure(failure["class"], reader)
        return _failure("malformed-output", "failure.reader")

    if receipt.get("status") != "verified":
        return _failure("malformed-output", "receipt.status")
    for field in REQUIRED_FIELDS:
        if field not in receipt:
            return _failure("malformed-output", f"receipt.{field}")

    session = receipt["session"]
    if not isinstance(session, dict) or not FULL_SESSION_ID.fullmatch(str(session.get("id", ""))):
        return _failure("identity-miss", "session.id")
    if session.get("harness") != "codex" or session.get("identity_constant") is not True:
        return _failure("identity-miss", "session.identity_constant")
    turn_ids = session.get("turn_ids")
    if not isinstance(turn_ids, list) or not turn_ids or any(not isinstance(turn, str) for turn in turn_ids):
        return _failure("malformed-output", "session.turn_ids")
    if receipt.get("continuation_owner") != "goal":
        return _failure("identity-miss", "receipt.continuation_owner")
    if receipt["user_message_count"] != 1:
        return _failure("malformed-output", "receipt.user_message_count")
    if not isinstance(receipt["correlation_id"], str) or not receipt["correlation_id"]:
        return _failure("malformed-output", "receipt.correlation_id")
    if not isinstance(receipt["action_hash"], str) or not receipt["action_hash"].startswith("sha256:"):
        return _failure("malformed-output", "receipt.action_hash")

    proof = receipt["proof"]
    if not isinstance(proof, dict):
        return _failure("malformed-output", "proof")
    for field in ("mail_count", "queue_count", "manual_submit_count"):
        if proof.get(field) != 0:
            return _failure("malformed-output", f"proof.{field}")
    if proof.get("native_goal_initially_absent") is not True:
        return _failure("malformed-output", "proof.native_goal_initially_absent")
    if proof.get("independent_stop") is not True:
        return _failure("parser-rejected", "stop.independent")
    if proof.get("goal_delegation") is not True:
        return _failure("parser-rejected", "stop.delegated")

    independent = _nested(receipt, "stop", "independent")
    if not isinstance(independent, dict):
        return _failure("malformed-output", "stop.independent")
    if independent.get("session_id") != session["id"] or independent.get("turn_id") not in turn_ids:
        return _failure("identity-miss", "stop.independent.identity")
    if independent.get("decision") != "allow" or independent.get("class") != "visitor":
        return _failure("parser-rejected", "stop.independent.decision")
    if independent.get("correlation_id") != receipt["correlation_id"]:
        return _failure("identity-miss", "stop.independent.correlation_id")
    if not str(independent["correlation_id"]).startswith(f"stop:{session['id']}:"):
        return _failure("identity-miss", "stop.independent.correlation_owner")
    if independent.get("goal_before") != "absent":
        return _failure("malformed-output", "stop.independent.goal_before")
    if independent.get("useful_action_after_stop") is not True:
        return _failure("parser-rejected", "stop.independent.useful_action_after_stop")
    if independent.get("continuation_owner") != "none":
        return _failure("parser-rejected", "stop.independent.owner")
    if independent.get("action_order") != ["stop-visitor", "goal-init", "goal-useful-action"]:
        return _failure("parser-rejected", "stop.independent.action_order")
    timestamps = [
        independent.get("first_step_at_ns"),
        independent.get("visitor_at_ns"),
        independent.get("goal_ensured_at_ns"),
        independent.get("manifest_written_at_ns"),
        independent.get("useful_action_at_ns"),
        _nested(receipt, "stop", "delegated", "stop_at_ns"),
    ]
    if any(type(stamp) is not int for stamp in timestamps) or timestamps != sorted(timestamps):
        return _failure("parser-rejected", "stop.independent.action_order")

    delegated = _nested(receipt, "stop", "delegated")
    if not isinstance(delegated, dict):
        return _failure("malformed-output", "stop.delegated")
    if delegated.get("session_id") != session["id"] or delegated.get("turn_id") not in turn_ids:
        return _failure("identity-miss", "stop.delegated.identity")
    if delegated.get("class") != "delegated-to-goal" or delegated.get("decision") != "allow":
        return _failure("parser-rejected", "stop.delegated.decision")
    if not isinstance(delegated.get("correlation_id"), str) or not delegated["correlation_id"].startswith(
        f"stop:{session['id']}:"
    ):
        return _failure("identity-miss", "stop.delegated.correlation_owner")
    if delegated.get("continuation_owner") != "goal" or delegated.get("block_count") != 0:
        return _failure("parser-rejected", "stop.delegated.owner")
    if delegated.get("useful_action") is not True:
        return _failure("parser-rejected", "stop.delegated.useful_action")
    if delegated.get("useful_action_at_ns") > delegated.get("stop_at_ns"):
        return _failure("parser-rejected", "stop.delegated.action_order")

    window = receipt["window"]
    if not isinstance(window, dict):
        return _failure("malformed-output", "window")
    if (window.get("requested"), window.get("max"), window.get("percent"), window.get("effective")) != (
        1_000_000,
        872_000,
        0.95,
        828_400,
    ):
        return _failure("resume-marker-stale", "resume.window")
    if window.get("default") != 272_000 or window.get("source") != "explicit-per-thread":
        return _failure("resume-marker-stale", "resume.window_source")
    if window.get("no_request_control_effective") != 258_400 or window.get("cost_policy") != "272K":
        return _failure("resume-marker-stale", "resume.window_control")

    goal = receipt["goal"]
    if not isinstance(goal, dict):
        return _failure("malformed-output", "goal")
    scope = goal.get("scope")
    if not isinstance(scope, str) or not scope:
        return _failure("malformed-output", "goal.scope")
    init = goal.get("init")
    if not isinstance(init, dict):
        return _failure("malformed-output", "goal.init")
    ensure = init.get("ensure_receipt")
    if (
        not isinstance(ensure, dict)
        or ensure.get("provider") != "codex"
        or ensure.get("status") != "active"
        or ensure.get("scope") != scope
        or ensure.get("thread_id") != session["id"]
    ):
        return _failure("identity-miss", "goal.init.ensure_receipt")
    objective = f"$fno:reign {scope}"
    if ensure.get("objective") != objective:
        return _failure("identity-miss", "goal.init.objective")
    manifest = init.get("manifest")
    if (
        not isinstance(manifest, dict)
        or manifest.get("written") is not True
        or manifest.get("scope") != scope
        or manifest.get("thread_id") != session["id"]
    ):
        return _failure("identity-miss", "goal.init.manifest")
    ensured_at = init.get("ensure_completed_at_ns")
    manifest_at = init.get("manifest_written_at_ns")
    if type(ensured_at) is not int or type(manifest_at) is not int or ensured_at >= manifest_at:
        return _failure("malformed-output", "goal.init.order")
    refused = init.get("refused_retry")
    if not isinstance(refused, dict) or refused.get("status") != "refused" or refused.get("manifest_unchanged") is not True:
        return _failure("identity-miss", "goal.init.refused_retry")
    unreadable = init.get("refused_no_goal")
    if (
        not isinstance(unreadable, dict)
        or unreadable.get("status") != "refused"
        or unreadable.get("thread_id") != session["id"]
        or unreadable.get("provider_goal_readable") is not False
        or unreadable.get("manifest_written") is not False
    ):
        return _failure("identity-miss", "goal.init.refused_no_goal")
    refusal = _nested(receipt, "proof", "provider_goal_refusal")
    if (
        not isinstance(refusal, dict)
        or refusal.get("thread_id") != session["id"]
        or refusal.get("scope") != unreadable.get("scope")
        or refusal.get("provider_goal_readable") is not False
        or refusal.get("manifest_absent") is not True
        or not isinstance(refusal.get("provider_error"), str)
        or not refusal["provider_error"].strip()
        or not isinstance(refusal.get("init_error"), str)
        or not refusal["init_error"].strip()
    ):
        return _failure("identity-miss", "goal.init.provider_refusal")
    before, after, paused, resumed = (goal.get(name) for name in ("before", "after", "paused", "resumed"))
    if before != {"status": "absent", "objective": None, "usage": None}:
        return _failure("identity-miss", "goal.before")
    before_receipt = goal.get("before_receipt")
    if not isinstance(before_receipt, str) or "no goal" not in before_receipt.lower():
        return _failure("identity-miss", "goal.before.provider_receipt")
    if not isinstance(after, dict) or after.get("status") != "active" or after.get("objective") != objective:
        return _failure("identity-miss", "goal.after")
    if after.get("thread_id") != session["id"]:
        return _failure("identity-miss", "goal.after.thread_id")
    if (
        not isinstance(paused, dict)
        or paused.get("status") != "paused"
        or paused.get("objective") != objective
        or paused.get("thread_id") != session["id"]
    ):
        return _failure("explicit-park", "goal.paused")
    if (
        not isinstance(resumed, dict)
        or resumed.get("status") != "active"
        or resumed.get("objective") != objective
        or resumed.get("thread_id") != session["id"]
    ):
        return _failure("wake-refused", "goal.resumed")
    usage_rows = [row.get("usage") for row in (after, paused, resumed)]
    if any(not isinstance(usage, dict) for usage in usage_rows):
        return _failure("explicit-park", "goal.usage")
    if any("token_budget" not in usage for usage in usage_rows):
        return _failure("malformed-output", "goal.usage.token_budget")
    if any(
        usage["token_budget"] is not None
        and (type(usage["token_budget"]) is not int or usage["token_budget"] < 0)
        for usage in usage_rows
    ):
        return _failure("malformed-output", "goal.usage.token_budget")
    if any(
        type(usage.get(field)) is not int or usage[field] < 0
        for usage in usage_rows
        for field in ("tokens_used", "time_used_seconds")
    ):
        return _failure("malformed-output", "goal.usage")
    if any(usage.get("token_budget") != usage_rows[0].get("token_budget") for usage in usage_rows[1:]):
        return _failure("explicit-park", "goal.token_budget")
    for before_usage, after_usage in zip(usage_rows, usage_rows[1:]):
        if any(
            after_usage[field] < before_usage[field]
            for field in ("tokens_used", "time_used_seconds")
        ):
            return _failure("explicit-park", "goal.usage")

    quiet = _nested(receipt, "proof", "quiet_park")
    if not isinstance(quiet, dict):
        return _failure("explicit-park", "quiet_park")
    if (
        quiet.get("session_id") != session["id"]
        or quiet.get("scope") != scope
        or quiet.get("park_count") != 1
        or quiet.get("stop_samples_during_hold") != 0
        or quiet.get("turns_during_hold") != 0
        or quiet.get("goal_usage_stable") is not True
    ):
        return _failure("explicit-park", "quiet_park.stop_samples_during_hold")
    paused_sample, held_sample = quiet.get("paused_goal_receipt"), quiet.get("held_goal_receipt")
    if (
        not isinstance(paused_sample, dict)
        or not isinstance(held_sample, dict)
        or paused_sample.get("thread_id") != session["id"]
        or held_sample.get("thread_id") != session["id"]
        or paused_sample.get("status") != "paused"
        or held_sample.get("status") != "paused"
        or not isinstance(paused_sample.get("usage"), dict)
        or paused_sample.get("usage") != held_sample.get("usage")
    ):
        return _failure("explicit-park", "quiet_park.goal_usage_stable")
    if (
        quiet.get("wake_result") != "resumed"
        or type(quiet.get("park_interval_seconds")) not in (int, float)
        or quiet["park_interval_seconds"] <= 0
    ):
        return _failure("wake-refused", "quiet_park.wake_result")
    wake = proof.get("wake_receipt")
    wake_goal = wake.get("provider_receipt") if isinstance(wake, dict) else None
    if (
        not isinstance(wake, dict)
        or wake.get("session_id") != session["id"]
        or wake.get("scope") != scope
        or wake.get("reason") != "board"
        or not isinstance(wake_goal, dict)
        or wake_goal.get("thread_id") != session["id"]
        or wake_goal.get("status") != "active"
    ):
        return _failure("wake-refused", "quiet_park.wake_receipt")
    compact = proof.get("compaction_receipt")
    if (
        not isinstance(compact, dict)
        or compact.get("verified") is not True
        or compact.get("provider") != "codex"
        or compact.get("action") != "compact"
        or compact.get("thread_id") != session["id"]
    ):
        return _failure("compaction-marker-stale", "compaction.provider_receipt")
    daemon = proof.get("private_daemon_replacement")
    if (
        not isinstance(daemon, dict)
        or daemon.get("status") != "verified"
        or daemon.get("session_id") != session["id"]
        or daemon.get("returncode") != 0
        or daemon.get("code_home_is_private") is not True
    ):
        return _failure("resume-marker-stale", "daemon.private_replacement")

    repeats = _nested(receipt, "proof", "repeats")
    if not isinstance(repeats, list) or {row.get("boundary") for row in repeats if isinstance(row, dict)} != {
        "compaction",
        "resume",
        "private-daemon-replacement",
    }:
        return _failure("compaction-marker-stale", "lifecycle.boundaries")
    for row in repeats:
        if not isinstance(row, dict) or row.get("status") != "verified" or row.get("same_session") is not True:
            boundary = row.get("boundary", "unknown") if isinstance(row, dict) else "unknown"
            return _failure(
                "compaction-marker-stale" if boundary == "compaction" else "resume-marker-stale",
                f"{boundary}.lifecycle_marker",
            )
        if row.get("useful_action") is not True:
            return _failure("parser-rejected", f"{row.get('boundary', 'unknown')}.useful_action")
        if (
            row.get("session_id") != session["id"]
            or row.get("turn_id") not in turn_ids
            or not str(row.get("correlation_id", "")).startswith(f"stop:{session['id']}:")
            or not str(row.get("action_hash", "")).startswith("sha256:")
        ):
            return _failure("identity-miss", f"{row.get('boundary', 'unknown')}.identity")

    return {
        "ok": True,
        "class": "verified-continuation",
        "failed_reader": None,
        "window": {"requested": window["requested"], "effective": window["effective"]},
    }


def _receipt_dir(root: Path) -> Path:
    path = root / "receipts"
    path.mkdir(parents=True, exist_ok=True)
    return path


def _private_environment(root: Path, repo: Path) -> dict[str, str]:
    root = root.resolve()
    paths = {
        "FNO_HOME": root,
        "FNO_AGENTS_HOME": root / "agents",
        "FNO_CLAIMS_ROOT": root / "claims",
        "FNO_SPACES_DIR": root / "spaces",
        "HOME": root / "home",
        "CODEX_HOME": root / "codex",
    }
    for path in paths.values():
        path.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    for name in (
        "FNO_CONFIG",
        "FNO_GRAPH_PATH",
        "FNO_LEDGER_PATH",
        "FNO_TARGET_STATE_PATH",
        "FNO_EVENTS_PATH",
        "GLOBAL_EVENTS_PATH",
        "FNO_AGENTS_BIN",
        "FNO_AGENTS_RUNTIME",
        "FNO_LOOPCHECK_FNO_BIN",
        "FNO_DRIVER_LIB_DIR",
        "FNO_HARNESS_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_SESSION_ID",
        "CODEX_PLUGIN_ROOT",
        "CLAUDE_PLUGIN_ROOT",
    ):
        env.pop(name, None)
    env.update({key: str(value) for key, value in paths.items()})
    env["FNO_REPO_ROOT"] = str(repo)
    env["FNO_SMOKE_ROOT"] = str(root)
    env["FNO_EVENTS_PATH"] = str(root / "events.jsonl")
    env["GLOBAL_EVENTS_PATH"] = str(root / "global-events.jsonl")
    env["FNO_HARNESS"] = "codex"
    return env


def _enable_private_king(repo: Path) -> None:
    config_dir = repo / ".fno"
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.toml").write_text("[king]\nenabled = true\n", encoding="utf-8")


def _write_agents_wrapper(root: Path, real_binary: str, env: dict[str, str]) -> str:
    """Capture the native readiness receipt that king init consumes."""
    wrapper_dir = root / "bin"
    wrapper_dir.mkdir(parents=True, exist_ok=True)
    receipt_path = root / "receipts" / "provider-readiness.jsonl"
    receipt_path.parent.mkdir(parents=True, exist_ok=True)
    wrapper = wrapper_dir / "fno-agents"
    wrapper.write_text(
        "#!/usr/bin/env python3\n"
        "import json, os, subprocess, sys, time\n"
        "real = os.environ['FNO_SMOKE_REAL_AGENTS_BIN']\n"
        "args = sys.argv[1:]\n"
        "result = subprocess.run([real, *args], capture_output=True, text=True)\n"
        "if args[:2] == ['loop', 'readiness'] and '--ensure-goal' in args:\n"
        "    row = {'args': args, 'returncode': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr, 'completed_at_ns': time.time_ns()}\n"
        "    path = os.path.join(os.environ['FNO_SMOKE_ROOT'], 'receipts', 'provider-readiness.jsonl')\n"
        "    with open(path, 'a', encoding='utf-8') as output:\n"
        "        output.write(json.dumps(row, sort_keys=True) + '\\n')\n"
        "sys.stdout.write(result.stdout)\n"
        "sys.stderr.write(result.stderr)\n"
        "raise SystemExit(result.returncode)\n",
        encoding="utf-8",
    )
    wrapper.chmod(0o700)
    env["FNO_SMOKE_REAL_AGENTS_BIN"] = real_binary
    env["FNO_AGENTS_BIN"] = str(wrapper)
    env["PATH"] = str(wrapper_dir) + os.pathsep + env.get("PATH", os.defpath)
    return str(wrapper)


def _run(argv: list[str], *, env: dict[str, str], cwd: Path, timeout: int = 120) -> subprocess.CompletedProcess[str]:
    return subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)


def _version(binary: str, env: dict[str, str], cwd: Path) -> str:
    result = _run([binary, "--version"], env=env, cwd=cwd, timeout=30)
    if result.returncode:
        raise RuntimeError(f"version probe failed for {binary}: {result.stderr.strip()}")
    return result.stdout.strip()


def _json_object(result: subprocess.CompletedProcess[str], label: str) -> dict[str, Any]:
    for line in reversed(result.stdout.splitlines()):
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            return value
    raise RuntimeError(f"malformed-output: {label} returned no JSON object")


def _provider_action(
    binary: str,
    *,
    env: dict[str, str],
    cwd: Path,
    session_id: str,
    scope: str,
    method: str,
    expected_action: str,
) -> dict[str, Any]:
    result = _run(
        [
            binary,
            "loop",
            "command",
            "--session",
            session_id,
            "--cwd",
            str(cwd),
            "--method",
            method,
            "--scope",
            scope,
        ],
        env=env,
        cwd=cwd,
        timeout=120,
    )
    if result.returncode:
        raise RuntimeError(
            f"provider-action-refused: {method}: {result.stderr.strip() or result.stdout.strip()}"
        )
    return _require_provider_receipt(
        _json_object(result, method), session_id=session_id, action=expected_action
    )


def _provider_goal_absence(
    binary: str, *, env: dict[str, str], cwd: Path, session_id: str, scope: str
) -> dict[str, Any]:
    result = _run(
        [
            binary,
            "loop",
            "command",
            "--session",
            session_id,
            "--cwd",
            str(cwd),
            "--method",
            "thread/goal/get",
            "--scope",
            scope,
        ],
        env=env,
        cwd=cwd,
        timeout=120,
    )
    detail = result.stderr.strip() or result.stdout.strip()
    if result.returncode == 0 or "no goal" not in detail.lower():
        raise RuntimeError(f"identity-miss: expected provider goal absence, got {detail}")
    return {"status": "absent", "objective": None, "usage": None, "receipt": detail}


def _window_receipt(binary: str, env: dict[str, str], cwd: Path) -> dict[str, Any]:
    def read(configured: int) -> dict[str, Any]:
        result = _run(
            [
                binary,
                "context-run",
                "--effective-window",
                "--model",
                "gpt-6-astra",
                "--configured",
                str(configured),
                "--cap",
                "872000",
                "--percent",
                "95",
            ],
            env=env,
            cwd=cwd,
            timeout=30,
        )
        if result.returncode:
            raise RuntimeError(f"resume-marker-stale: effective-window API refused {configured}")
        return _json_object(result, "effective-window")

    explicit = read(1_000_000)
    control = read(272_000)
    if explicit.get("effective") != 828_400 or control.get("effective") != 258_400:
        raise RuntimeError("resume-marker-stale: effective-window provider receipts differ")
    return {
        "requested": explicit["configured"],
        "default": control["configured"],
        "max": explicit["max_context_window"],
        "percent": explicit["percent"] / 100,
        "effective": explicit["effective"],
        "source": "explicit-per-thread",
        "no_request_control_effective": control["effective"],
        "cost_policy": "272K",
        "provider_receipts": [explicit, control],
    }


def _event_rows(root: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    seen: set[tuple[Any, ...]] = set()
    for event_path in root.rglob("*.jsonl"):
        try:
            lines = event_path.read_text(encoding="utf-8", errors="replace").splitlines()
        except OSError:
            continue
        for line in lines:
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(row, dict):
                continue
            data = row.get("data") if isinstance(row.get("data"), dict) else {}
            key = (
                row.get("type"),
                data.get("session_id"),
                data.get("turn_id"),
                data.get("correlation_id"),
                data.get("scope"),
                row.get("ts") or row.get("created_at"),
            )
            if any(part is not None for part in key) and key in seen:
                continue
            if any(part is not None for part in key):
                seen.add(key)
            rows.append(row)
    return rows


def _event_data(row: dict[str, Any]) -> dict[str, Any]:
    data = row.get("data")
    return data if isinstance(data, dict) else {}


def _event_time_ns(row: dict[str, Any]) -> int:
    stamp = row.get("ts") or row.get("created_at")
    if isinstance(stamp, (int, float)):
        return int(stamp * 1_000_000_000)
    if isinstance(stamp, str):
        try:
            return int(dt.datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp() * 1_000_000_000)
        except ValueError:
            pass
    raise RuntimeError("malformed-output: event receipt has no parseable timestamp")


def _stop_events(rows: list[dict[str, Any]], session_id: str) -> list[dict[str, Any]]:
    events = [
        row
        for row in rows
        if row.get("type") == "stop_decision"
        and _event_data(row).get("session_id") == session_id
    ]
    return sorted(events, key=_event_time_ns)


def _ensure_receipt(root: Path, session_id: str, scope: str) -> dict[str, Any]:
    path = root / "receipts" / "provider-readiness.jsonl"
    try:
        rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line]
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeError(f"malformed-output: provider readiness receipts unreadable: {error}") from error
    for row in reversed(rows):
        args = row.get("args")
        if not isinstance(args, list) or "--ensure-goal" not in args:
            continue
        if _flag_value(args, "--session") != session_id or _flag_value(args, "--scope") != scope:
            continue
        result = _json_object(
            subprocess.CompletedProcess([], row.get("returncode", 1), row.get("stdout", ""), row.get("stderr", "")),
            "provider readiness",
        )
        receipt = result.get("goal_receipt")
        if row.get("returncode") != 0 or not isinstance(receipt, dict):
            raise RuntimeError("identity-miss: king init did not return a provider goal receipt")
        return {"receipt": receipt, "completed_at_ns": row.get("completed_at_ns")}
    raise RuntimeError("malformed-output: king init produced no ensure-goal provider receipt")


def _flag_value(args: list[Any], flag: str) -> str | None:
    try:
        index = args.index(flag)
        value = args[index + 1]
    except (ValueError, IndexError):
        return None
    return value if isinstance(value, str) else None


def _repo_fixture(root: Path) -> Path:
    repo = root / "repo"
    repo.mkdir(parents=True, exist_ok=True)
    for relative in ("first-step.txt", "nonce.txt"):
        path = repo / relative
        if path.exists():
            path.unlink()
    init = subprocess.run(["git", "init", "-q", str(repo)], capture_output=True, text=True)
    if init.returncode:
        raise RuntimeError(f"private git init failed: {init.stderr.strip()}")
    (repo / "README.md").write_text("disposable continuation fixture\n", encoding="utf-8")
    stage = subprocess.run(
        ["git", "-C", str(repo), "add", "-A"],
        capture_output=True,
        text=True,
    )
    if stage.returncode:
        raise RuntimeError(f"private git fixture staging failed: {stage.stderr.strip()}")
    commit = subprocess.run(
        ["git", "-C", str(repo), "-c", "user.email=smoke@example.test", "-c", "user.name=smoke", "commit", "-qm", "fixture"],
        capture_output=True,
        text=True,
    )
    if commit.returncode:
        raise RuntimeError(f"private git fixture commit failed: {commit.stderr.strip()}")
    return repo


def _json_events(output: str, label: str) -> list[dict[str, Any]]:
    events = []
    for line in output.splitlines():
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError as error:
            raise RuntimeError(f"malformed-output: {label} JSON event: {error}") from error
        if isinstance(event, dict):
            events.append(event)
    return events


def _run_codex(
    codex: str,
    repo: Path,
    env: dict[str, str],
    *,
    session_id: str | None = None,
    prompt: str = "",
) -> list[dict[str, Any]]:
    flags = ["--dangerously-bypass-approvals-and-sandbox", "--dangerously-bypass-hook-trust"]
    if session_id is None:
        argv = [codex, "exec", "--json", *flags, "--cd", str(repo), prompt]
    else:
        env["CODEX_THREAD_ID"] = session_id
        argv = [codex, "exec", "resume", "--json", *flags, session_id]
    result = _run(argv, env=env, cwd=repo, timeout=900)
    if result.returncode:
        raise RuntimeError(f"private Codex journey failed: {result.stderr[-1200:].strip()}")
    return _json_events(result.stdout, "private Codex journey")


def _new_scope(fno: str, repo: Path, env: dict[str, str], title: str, *, parent: str | None = None) -> str:
    argv = [fno, "backlog", "idea", title, "--type", "epic" if parent is None else "feature", "--project", "fno", "--cwd", str(repo), "--source-kind", "operator_request", "--json"]
    if parent:
        argv.extend(["--parent", parent])
    result = _run(argv, env=env, cwd=repo, timeout=120)
    if result.returncode:
        raise RuntimeError(f"private backlog fixture refused: {result.stderr.strip() or result.stdout.strip()}")
    match = re.search(r"\b[a-z][a-z0-9]*-[0-9a-f]{4,}\b", result.stdout)
    if not match:
        raise RuntimeError("malformed-output: private backlog fixture returned no node id")
    return match.group(0)


def _turn_ids(events: list[dict[str, Any]]) -> list[str]:
    return list(dict.fromkeys(
        event.get("turn_id") for event in events
        if event.get("type") == "turn.started" and isinstance(event.get("turn_id"), str)
    ))


def _session_turns(rows: list[dict[str, Any]], session_id: str) -> set[str]:
    return {
        str(data["turn_id"])
        for row in rows
        if (data := _event_data(row)).get("session_id") == session_id
        and isinstance(data.get("turn_id"), str)
    }


def _stop_after(
    rows: list[dict[str, Any]],
    session_id: str,
    after_ns: int,
    klass: str,
    *,
    action_ns: int = 0,
) -> dict[str, Any]:
    boundary_ns = max(after_ns, action_ns)
    matches = [
        row for row in _stop_events(rows, session_id)
        if _event_time_ns(row) >= boundary_ns
        and _event_data(row).get("decision") == "allow"
        and _event_data(row).get("class") == klass
    ]
    if not matches:
        raise RuntimeError(f"parser-rejected: no {klass} Stop followed the private action")
    return matches[0]


def _append_proof(
    codex: str,
    repo: Path,
    env: dict[str, str],
    session_id: str,
    nonce: Path,
    boundary: str,
) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    before = hashlib.sha256(nonce.read_bytes()).hexdigest() if nonce.exists() else ""
    started = time.time_ns()
    events = _run_codex(codex, repo, env, session_id=session_id)
    if any(event.get("type") == "user_message" for event in events):
        raise RuntimeError("parser-rejected: resume supplied another user message")
    after = hashlib.sha256(nonce.read_bytes()).hexdigest() if nonce.exists() else ""
    if not after or after == before:
        raise RuntimeError(f"parser-rejected: {boundary} produced no useful nonce action")
    rows = _event_rows(Path(env["FNO_SMOKE_ROOT"]))
    stop = _stop_after(
        rows, session_id, started, "delegated-to-goal", action_ns=nonce.stat().st_mtime_ns
    )
    data = _event_data(stop)
    turn_id = data.get("turn_id")
    return (
        {
            "boundary": boundary,
            "status": "verified",
            "same_session": data.get("session_id") == session_id,
            "useful_action": True,
            "session_id": session_id,
            "turn_id": turn_id,
            "correlation_id": data.get("correlation_id"),
            "action_hash": f"sha256:{after}",
        },
        events,
    )


def _run_journey(root: Path) -> Path:
    run_root = root / f"run-{uuid.uuid4().hex}"
    run_root.mkdir(parents=True)
    repo = _repo_fixture(run_root)
    _enable_private_king(repo)
    env = _private_environment(run_root, repo)
    codex = os.environ.get("CODEX_BIN") or shutil.which("codex")
    if not codex:
        raise RuntimeError("external dependency missing: codex is not available")
    repo_root = Path(__file__).resolve().parents[2]
    native_root = repo_root / "crates" / "fno-agents" / "target" / "debug" / "fno-agents"
    fno_agents = str(native_root) if native_root.is_file() else os.environ.get("FNO_AGENTS_BIN") or shutil.which("fno-agents")
    if not fno_agents:
        raise RuntimeError("external dependency missing: fno-agents is not available")
    fno = shutil.which("fno")
    if not fno:
        raise RuntimeError("external dependency missing: fno is not available")
    source_path = str(repo_root / "cli" / "src")
    env["PYTHONPATH"] = os.pathsep.join(filter(None, (source_path, env.get("PYTHONPATH", ""))))
    _write_agents_wrapper(run_root, fno_agents, env)
    versions = {"codex": _version(codex, env, repo), "fno_agents": _version(fno_agents, env, repo)}
    versions["fno"] = _version(fno, env, repo)
    scope = _new_scope(fno, repo, env, "EPIC: private Codex continuation proof")
    obligation = _new_scope(fno, repo, env, "unplanned continuation obligation", parent=scope)
    refusal_scope = _new_scope(fno, repo, env, "EPIC: unreadable provider proof")
    marketplace = _run(
        [codex, "plugin", "marketplace", "add", str(repo_root), "--json"],
        env=env,
        cwd=repo,
        timeout=180,
    )
    if marketplace.returncode:
        raise RuntimeError(f"private plugin marketplace refused: {marketplace.stderr.strip() or marketplace.stdout.strip()}")
    install = _run(
        [codex, "plugin", "add", "fno@footnote", "--json"],
        env=env,
        cwd=repo,
        timeout=180,
    )
    if install.returncode:
        raise RuntimeError(f"private plugin install refused: {install.stderr.strip() or install.stdout.strip()}")

    prompt = (
        "This is a disposable continuation proof. Perform one first step by "
        "writing first-step.txt, then stop. Do not create nonce.txt during this "
        "first turn. If this same thread later continues without a new user "
        "message, append the next numbered line to nonce.txt and stop. Each "
        "automatic continuation should append one line. Do not ask for input."
    )
    events = _run_codex(codex, repo, env, prompt=prompt)
    thread_id = next((event.get("thread_id") for event in events if event.get("type") == "thread.started"), None)
    first_turn = _turn_ids(events)
    if not thread_id or not first_turn or sum(event.get("type") == "user_message" for event in events) != 1:
        raise RuntimeError("parser-rejected: private Codex rollout lacks one full thread, turn, and user message")
    nonce, first_step = repo / "nonce.txt", repo / "first-step.txt"
    if not first_step.is_file() or nonce.exists():
        raise RuntimeError("parser-rejected: first turn did not stay independent")
    rows = _event_rows(run_root)
    visitor = _stop_after(rows, thread_id, 0, "visitor", action_ns=first_step.stat().st_mtime_ns)
    visitor_data = _event_data(visitor)
    visitor_at_ns = _event_time_ns(visitor)
    first_step_at_ns = first_step.stat().st_mtime_ns
    if visitor_data.get("decision") != "allow" or visitor_data.get("continuation_owner") != "none":
        raise RuntimeError("parser-rejected: initial Stop was not an unowned visitor stop")
    correlation = visitor_data.get("correlation_id")
    if not isinstance(correlation, str) or not correlation.startswith(f"stop:{thread_id}:"):
        raise RuntimeError("identity-miss: visitor Stop correlation does not name its thread")
    before_goal = _provider_goal_absence(fno_agents, env=env, cwd=repo, session_id=thread_id, scope=scope)
    if before_goal.get("status") != "absent":
        raise RuntimeError("identity-miss: native goal was present before king init")

    adopted = _run([fno, "agents", "adopt", thread_id, "--cross-project"], env=env, cwd=repo)
    if adopted.returncode or not adopted.stdout.strip():
        raise RuntimeError(f"private Codex session adoption refused: {adopted.stderr.strip()}")
    crown_name = adopted.stdout.strip().splitlines()[0]
    crown = _run([fno, "agents", "crown", crown_name, "--scope", scope], env=env, cwd=repo)
    if crown.returncode:
        raise RuntimeError(f"private crown refused: {crown.stderr.strip() or crown.stdout.strip()}")
    init = _run([fno, "agents", "king", "init", "--scope", scope, "--harness-session-id", thread_id], env=env, cwd=repo)
    if init.returncode:
        raise RuntimeError(f"private king init refused: {init.stderr.strip() or init.stdout.strip()}")
    match = re.search(r"king: manifest written: (.+)", init.stdout)
    if not match:
        raise RuntimeError("malformed-output: king init returned no manifest path")
    manifest = Path(match.group(1).strip())
    manifest_stat = manifest.stat()
    ensure = _ensure_receipt(run_root, thread_id, scope)
    if not visitor_at_ns < ensure["completed_at_ns"] < manifest_stat.st_mtime_ns:
        raise RuntimeError("parser-rejected: crown manifest predates provider goal ensure")
    ensure_receipt = ensure["receipt"]
    after_goal = _provider_action(
        fno_agents, env=env, cwd=repo, session_id=thread_id, scope=scope,
        method="thread/goal/get", expected_action="goal_get",
    )
    manifest_before_retry = hashlib.sha256(manifest.read_bytes()).hexdigest()
    refused_retry = _run([fno, "agents", "king", "init", "--scope", scope, "--harness-session-id", thread_id], env=env, cwd=repo)
    retry_unchanged = hashlib.sha256(manifest.read_bytes()).hexdigest() == manifest_before_retry
    if refused_retry.returncode == 0:
        raise RuntimeError("parser-rejected: repeated king init unexpectedly replaced the crown")
    if not retry_unchanged:
        raise RuntimeError("identity-miss: repeated king init changed the existing crown")

    unreadable_env = env.copy()
    unreadable_env["CODEX_HOME"] = str(run_root / "codex-unreadable")
    Path(unreadable_env["CODEX_HOME"]).mkdir(parents=True, exist_ok=True)
    refused_manifest = manifest.parent / f"{refusal_scope}.md"
    provider_probe = _run(
        [fno_agents, "loop", "command", "--session", thread_id, "--cwd", str(repo), "--method", "thread/goal/get", "--scope", refusal_scope],
        env=unreadable_env, cwd=repo,
    )
    refused_no_goal = _run(
        [fno, "agents", "king", "init", "--scope", refusal_scope, "--harness-session-id", thread_id],
        env=unreadable_env, cwd=repo,
    )
    provider_error = provider_probe.stderr.strip() or provider_probe.stdout.strip()
    if (
        provider_probe.returncode == 0
        or not re.search(r"goal|thread|session|app-server", provider_error, re.IGNORECASE)
        or refused_no_goal.returncode == 0
        or refused_manifest.exists()
    ):
        raise RuntimeError("identity-miss: unreadable provider goal did not refuse a goalless crown")
    if hashlib.sha256(manifest.read_bytes()).hexdigest() != manifest_before_retry:
        raise RuntimeError("identity-miss: refused king init changed the existing crown")

    window = _window_receipt(fno_agents, env, repo)
    first_action_start = time.time_ns()
    first_resume = _run_codex(codex, repo, env, session_id=thread_id)
    user_message_count = 1 + sum(event.get("type") == "user_message" for event in first_resume)
    if user_message_count != 1 or not nonce.is_file():
        raise RuntimeError("parser-rejected: goal continuation needs a useful action and no new prompt")
    rows = _event_rows(run_root)
    useful_action_at_ns = nonce.stat().st_mtime_ns
    if not manifest_stat.st_mtime_ns < useful_action_at_ns:
        raise RuntimeError("parser-rejected: useful goal action did not follow king init")
    delegated = _stop_after(
        rows, thread_id, first_action_start, "delegated-to-goal", action_ns=useful_action_at_ns
    )
    delegated_data = _event_data(delegated)
    delegated_at_ns = _event_time_ns(delegated)
    if delegated_data.get("continuation_owner") != "goal":
        raise RuntimeError("identity-miss: later Stop did not delegate to the native goal")
    turns = list(dict.fromkeys(first_turn + _turn_ids(first_resume) + [visitor_data["turn_id"], delegated_data["turn_id"]]))
    quiet_events = [row for row in rows if row.get("type") == "quiet-undelivered" and _event_data(row).get("session_id") == thread_id and _event_data(row).get("scope") == scope]
    deadline = time.monotonic() + 20
    while not quiet_events and time.monotonic() < deadline:
        time.sleep(0.25)
        rows = _event_rows(run_root)
        quiet_events = [row for row in rows if row.get("type") == "quiet-undelivered" and _event_data(row).get("session_id") == thread_id and _event_data(row).get("scope") == scope]
    if len(quiet_events) != 1:
        raise RuntimeError("explicit-park: private king did not emit one exact-scope quiet park")
    quiet = quiet_events[0]
    quiet_data = _event_data(quiet)
    paused_receipt = quiet_data.get("provider_receipt")
    if not isinstance(paused_receipt, dict) or paused_receipt.get("thread_id") != thread_id or paused_receipt.get("status") != "paused":
        raise RuntimeError("identity-miss: quiet park did not pause the same provider goal")
    pause_started = _event_time_ns(quiet) / 1_000_000_000
    stop_count = len(_stop_events(rows, thread_id))
    turn_samples = _session_turns(rows, thread_id)
    paused_goal = _provider_action(fno_agents, env=env, cwd=repo, session_id=thread_id, scope=scope, method="thread/goal/get", expected_action="goal_get")
    time.sleep(2)
    held_rows = _event_rows(run_root)
    held_goal = _provider_action(fno_agents, env=env, cwd=repo, session_id=thread_id, scope=scope, method="thread/goal/get", expected_action="goal_get")
    stop_delta = len(_stop_events(held_rows, thread_id)) - stop_count
    turn_delta = len(_session_turns(held_rows, thread_id) - turn_samples)
    if stop_delta or turn_delta or held_goal.get("usage") != paused_goal.get("usage"):
        raise RuntimeError("explicit-park: paused Codex goal consumed another turn during hold")

    for node in (obligation, scope):
        closed = _run([fno, "backlog", "update", node, "--status", "done"], env=env, cwd=repo)
        if closed.returncode:
            raise RuntimeError(f"private wake board update refused: {closed.stderr.strip() or closed.stdout.strip()}")
    wake_started = time.time()
    env["FNO_HARNESS_SESSION_ID"] = thread_id
    wake = _run(
        [fno_agents, "loop", "run", "--driver", "king", "--scope", scope, "--cwd", str(repo), "--wake", "--wake-reason", "board", "--wake-detail", "private board changed"],
        env=env, cwd=repo,
    )
    if wake.returncode:
        raise RuntimeError(f"private board wake refused: {wake.stderr.strip() or wake.stdout.strip()}")
    rows = _event_rows(run_root)
    wake_rows = [row for row in rows if row.get("type") == "king_goal_resumed" and _event_data(row).get("session_id") == thread_id and _event_data(row).get("scope") == scope]
    if len(wake_rows) != 1 or _event_data(wake_rows[0]).get("reason") != "board":
        raise RuntimeError("wake-refused: no single exact-session board-change resume receipt")
    resumed_receipt = _event_data(wake_rows[0]).get("provider_receipt")
    if not isinstance(resumed_receipt, dict) or resumed_receipt.get("status") != "active" or resumed_receipt.get("thread_id") != thread_id:
        raise RuntimeError("identity-miss: board wake resumed a different provider goal")
    repeats = []
    row, wake_events = _append_proof(codex, repo, env, thread_id, nonce, "resume")
    repeats.append(row)
    turns.extend(_turn_ids(wake_events) + [row["turn_id"]])

    compact = _provider_action(fno_agents, env=env, cwd=repo, session_id=thread_id, scope=scope, method="thread/compact/start", expected_action="compact")
    requests = [
        {"method": "thread/goal/get", "status": "absent", "thread_id": thread_id},
        {"method": "thread/goal/get", "status": "verified", "thread_id": thread_id},
        {"method": "thread/compact/start", "status": "verified", "thread_id": thread_id},
        {"method": "king_goal_resumed", "status": "verified", "thread_id": thread_id},
    ]
    row, compact_events = _append_proof(codex, repo, env, thread_id, nonce, "compaction")
    repeats.append(row)
    turns.extend(_turn_ids(compact_events) + [row["turn_id"]])
    row, resume_events = _append_proof(codex, repo, env, thread_id, nonce, "resume")
    repeats.append(row)
    turns.extend(_turn_ids(resume_events) + [row["turn_id"]])
    daemon_restart = _run([codex, "app-server", "daemon", "restart"], env=env, cwd=repo, timeout=120)
    if daemon_restart.returncode:
        raise RuntimeError(f"private Codex daemon restart failed: {daemon_restart.stderr.strip()}")
    row, daemon_events = _append_proof(codex, repo, env, thread_id, nonce, "private-daemon-replacement")
    repeats.append(row)
    turns.extend(_turn_ids(daemon_events) + [row["turn_id"]])

    all_rows = _event_rows(run_root)
    stop_rows = _stop_events(all_rows, thread_id)
    if not turns or any(not isinstance(turn, str) for turn in turns):
        raise RuntimeError("identity-miss: private continuation turn ids are incomplete")
    turn_ids = list(dict.fromkeys(turns))
    action_hash = f"sha256:{hashlib.sha256(nonce.read_bytes()).hexdigest()}"
    receipt = {
        "schema_version": RECEIPT_SCHEMA_VERSION,
        "created_at": dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z"),
        "versions": versions,
        "session": {"id": thread_id, "harness": "codex", "turn_ids": turn_ids, "identity_constant": True},
        "correlation_id": correlation,
        "continuation_owner": "goal",
        "action_hash": action_hash,
        "user_message_count": user_message_count,
        "command_requests": requests,
        "window": window,
        "goal": {
            "scope": scope,
            "before": {"status": "absent", "objective": None, "usage": None},
            "before_receipt": before_goal["receipt"],
            "init": {
                "ensure_receipt": ensure_receipt,
                "ensure_completed_at_ns": ensure["completed_at_ns"],
                "manifest_written_at_ns": manifest_stat.st_mtime_ns,
                "manifest": {"written": True, "scope": scope, "thread_id": thread_id},
                "refused_retry": {"status": "refused", "manifest_unchanged": retry_unchanged},
                "refused_no_goal": {"status": "refused", "thread_id": thread_id, "scope": refusal_scope, "provider_goal_readable": False, "manifest_written": False},
            },
            "after": after_goal,
            "paused": paused_receipt,
            "resumed": resumed_receipt,
        },
        "stop": {
            "independent": {"decision": visitor_data["decision"], "class": visitor_data["class"], "continuation_owner": visitor_data.get("continuation_owner", "none"), "session_id": thread_id, "turn_id": visitor_data["turn_id"], "correlation_id": correlation, "goal_before": "absent", "useful_action_after_stop": nonce.exists(), "action_order": ["stop-visitor", "goal-init", "goal-useful-action"], "first_step_at_ns": first_step_at_ns, "visitor_at_ns": visitor_at_ns, "goal_ensured_at_ns": ensure["completed_at_ns"], "manifest_written_at_ns": manifest_stat.st_mtime_ns, "useful_action_at_ns": useful_action_at_ns},
            "delegated": {"decision": delegated_data["decision"], "class": delegated_data["class"], "continuation_owner": delegated_data.get("continuation_owner"), "session_id": thread_id, "turn_id": delegated_data["turn_id"], "correlation_id": delegated_data["correlation_id"], "block_count": sum(_event_data(item).get("decision") == "block" for item in stop_rows), "useful_action": nonce.exists(), "useful_action_at_ns": useful_action_at_ns, "stop_at_ns": delegated_at_ns},
        },
        "proof": {
            "mail_count": 0, "queue_count": 0, "manual_submit_count": 0,
            "native_goal_initially_absent": True, "independent_stop": True, "goal_delegation": True,
            "quiet_park": {"session_id": thread_id, "scope": scope, "park_count": len(quiet_events), "stop_samples_during_hold": stop_delta, "turns_during_hold": turn_delta, "goal_usage_stable": True, "paused_goal_receipt": paused_goal, "held_goal_receipt": held_goal, "park_interval_seconds": max(0.001, wake_started - pause_started), "wake_result": "resumed"},
            "wake_receipt": {"session_id": thread_id, "scope": scope, "reason": "board", "provider_receipt": resumed_receipt},
            "compaction_receipt": compact,
            "private_daemon_replacement": {"command": "codex app-server daemon restart", "status": "verified", "session_id": thread_id, "returncode": daemon_restart.returncode, "code_home_is_private": Path(env["CODEX_HOME"]).resolve().is_relative_to(Path(run_root).resolve())},
            "repeats": repeats,
            "provider_goal_refusal": {"thread_id": thread_id, "scope": refusal_scope, "provider_goal_readable": False, "provider_error": provider_error, "init_error": refused_no_goal.stderr.strip() or refused_no_goal.stdout.strip(), "manifest_absent": not refused_manifest.exists()},
        },
        "status": "verified",
    }
    result = classify_receipt(receipt)
    if not result["ok"]:
        raise RuntimeError(f"parser-rejected: receipt failed {result['failed_reader']}")
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    path = _receipt_dir(root) / f"{RECEIPT_PREFIX}{stamp}-{uuid.uuid4().hex[:8]}.json"
    path.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return path


def _receipt_age_hours(path: Path, now: float | None = None) -> float | None:
    try:
        stamp = json.loads(path.read_text(encoding="utf-8"))["created_at"]
        created = dt.datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()
    except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError):
        return None
    return ((time.time() if now is None else now) - created) / 3600


def verify_latest(root: Path, max_age_hours: float) -> int:
    receipts = sorted(_receipt_dir(root).glob(f"{RECEIPT_PREFIX}*.json"))
    if not receipts:
        print(f"codex_reign_continuation_not_ready: no receipt under {_receipt_dir(root)}", file=sys.stderr)
        return 1
    path = receipts[-1]
    age = _receipt_age_hours(path)
    if age is None or age < -0.01 or age > max_age_hours:
        print(f"codex_reign_continuation_not_ready: stale or unreadable receipt {path}", file=sys.stderr)
        return 1
    try:
        receipt = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        print(f"codex_reign_continuation_not_ready: {error}", file=sys.stderr)
        return 1
    result = classify_receipt(receipt)
    if not result["ok"]:
        print(
            f"codex_reign_continuation_not_ready: class={result['class']} reader={result['failed_reader']}",
            file=sys.stderr,
        )
        return 1
    print(f"codex_reign_continuation_verified session={receipt['session']['id']} action={receipt['action_hash']}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument("--run", action="store_true")
    modes.add_argument("--verify-latest", action="store_true")
    parser.add_argument("--max-age-hours", type=float, default=24.0)
    parser.add_argument("--root", type=Path, default=Path(os.environ.get("FNO_SMOKE_ROOT", DEFAULT_ROOT)))
    args = parser.parse_args(argv)
    args.root.mkdir(parents=True, exist_ok=True)
    if args.run:
        try:
            _run_journey(args.root.resolve())
        except (OSError, RuntimeError, subprocess.SubprocessError) as error:
            print(f"codex_reign_continuation_blocked: {error}", file=sys.stderr)
            return 2
        return 0
    return verify_latest(args.root.resolve(), args.max_age_hours)


if __name__ == "__main__":
    raise SystemExit(main())
