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
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


DEFAULT_ROOT = Path("/private/tmp/fno-continuation-proof")
RECEIPT_PREFIX = "codex_reign_continuation_"
RECEIPT_SCHEMA_VERSION = 2
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
    if independent.get("decision") != "block" or independent.get("class") != "actionable-block":
        return _failure("parser-rejected", "stop.independent.decision")
    if independent.get("correlation_id") != receipt["correlation_id"]:
        return _failure("identity-miss", "stop.independent.correlation_id")
    if independent.get("goal_before") != "absent":
        return _failure("malformed-output", "stop.independent.goal_before")
    if independent.get("useful_action_after_block") is not True:
        return _failure("parser-rejected", "stop.independent.useful_action_after_block")
    if independent.get("action_order") != ["stop-block", "nonce-write"]:
        return _failure("parser-rejected", "stop.independent.action_order")

    delegated = _nested(receipt, "stop", "delegated")
    if not isinstance(delegated, dict):
        return _failure("malformed-output", "stop.delegated")
    if delegated.get("class") != "delegated-to-goal" or delegated.get("decision") != "allow":
        return _failure("parser-rejected", "stop.delegated.decision")
    if delegated.get("continuation_owner") != "goal" or delegated.get("block_count") != 0:
        return _failure("parser-rejected", "stop.delegated.owner")
    if delegated.get("useful_action") is not True:
        return _failure("parser-rejected", "stop.delegated.useful_action")

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
    if window.get("no_request_control_effective") != 258_400 or window.get("cost_policy") != "272K":
        return _failure("resume-marker-stale", "resume.window_control")

    goal = receipt["goal"]
    if not isinstance(goal, dict):
        return _failure("malformed-output", "goal")
    before, after, paused, resumed = (goal.get(name) for name in ("before", "after", "paused", "resumed"))
    objective = "$fno:reign disposable"
    if before != {"status": "absent", "objective": None, "usage": 0}:
        return _failure("identity-miss", "goal.before")
    if not isinstance(after, dict) or after.get("status") != "active" or after.get("objective") != objective:
        return _failure("identity-miss", "goal.after")
    if not isinstance(paused, dict) or paused.get("status") != "paused" or paused.get("objective") != objective:
        return _failure("explicit-park", "goal.paused")
    if not isinstance(resumed, dict) or resumed.get("status") != "active" or resumed.get("objective") != objective:
        return _failure("wake-refused", "goal.resumed")
    if paused.get("usage") != after.get("usage") or resumed.get("usage") != paused.get("usage"):
        return _failure("explicit-park", "goal.usage")

    quiet = _nested(receipt, "proof", "quiet_park")
    if not isinstance(quiet, dict):
        return _failure("explicit-park", "quiet_park")
    if quiet.get("park_count") != 1 or quiet.get("stop_samples_during_hold") != 0:
        return _failure("explicit-park", "quiet_park.stop_samples_during_hold")
    if quiet.get("wake_result") != "resumed" or not isinstance(quiet.get("park_interval_seconds"), (int, float)):
        return _failure("wake-refused", "quiet_park.wake_result")

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
    env.update({key: str(value) for key, value in paths.items()})
    env["FNO_REPO_ROOT"] = str(repo)
    env["FNO_SMOKE_ROOT"] = str(root)
    return env


def _run(argv: list[str], *, env: dict[str, str], cwd: Path, timeout: int = 120) -> subprocess.CompletedProcess[str]:
    return subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)


def _version(binary: str, env: dict[str, str], cwd: Path) -> str:
    result = _run([binary, "--version"], env=env, cwd=cwd, timeout=30)
    if result.returncode:
        raise RuntimeError(f"version probe failed for {binary}: {result.stderr.strip()}")
    return result.stdout.strip()


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


def _run_journey(root: Path) -> Path:
    repo = _repo_fixture(root)
    env = _private_environment(root, repo)
    codex = os.environ.get("CODEX_BIN") or shutil.which("codex")
    if not codex:
        raise RuntimeError("external dependency missing: codex is not available")
    fno_agents = os.environ.get("FNO_AGENTS_BIN") or shutil.which("fno-agents")
    if not fno_agents:
        raise RuntimeError("external dependency missing: fno-agents is not available")

    _version(codex, env, repo)
    _version(fno_agents, env, repo)
    native_root = Path(__file__).resolve().parents[2] / "crates" / "fno-agents" / "target" / "debug" / "fno-agents"
    if native_root.is_file():
        _version(str(native_root), env, repo)

    repo_root = Path(__file__).resolve().parents[2]
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

    nonce = repo / "nonce.txt"
    prompt = (
        "This is a disposable continuation proof. Perform exactly one first step by "
        "writing first-step.txt in the repository, then end the turn. Do not write "
        "nonce.txt during that first step. If the Footnote Stop continuation runs "
        "without another user message, write nonce.txt and stop. Never ask for "
        "another user message."
    )
    run = _run(
        [
            codex,
            "exec",
            "--json",
            "--dangerously-bypass-approvals-and-sandbox",
            "--dangerously-bypass-hook-trust",
            "--cd",
            str(repo),
            prompt,
        ],
        env=env,
        cwd=repo,
        timeout=900,
    )
    if run.returncode:
        raise RuntimeError(f"private Codex journey failed: {run.stderr[-1200:].strip()}")
    events = [json.loads(line) for line in run.stdout.splitlines() if line.startswith("{")]
    user_messages = sum(1 for event in events if event.get("type") == "user_message")
    thread_id = next((event.get("thread_id") for event in events if event.get("type") == "thread.started"), None)
    turn_id = next((event.get("turn_id") for event in events if event.get("type") == "turn.started"), None)
    if not thread_id or not turn_id or user_messages != 1:
        raise RuntimeError("parser-rejected: private Codex rollout lacks one full thread, turn, and user message")
    if not nonce.exists():
        raise RuntimeError("parser-rejected: no useful nonce-writing continuation action observed")

    event_rows = []
    for event_path in root.rglob("*.jsonl"):
        try:
            for line in event_path.read_text(encoding="utf-8", errors="replace").splitlines():
                try:
                    row = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(row, dict):
                    event_rows.append(row)
        except OSError:
            continue
    stop_events = []
    for row in event_rows:
        data = row.get("data")
        if not isinstance(data, dict):
            continue
        if (
            row.get("type") == "stop_decision"
            and data.get("session_id") == thread_id
            and data.get("turn_id") == turn_id
            and data.get("decision") == "block"
            and data.get("class") == "actionable-block"
            and isinstance(data.get("correlation_id"), str)
        ):
            stop_events.append(row)
    if len(stop_events) != 1:
        raise RuntimeError(
            "parser-rejected: no unique actionable Stop receipt matched the exact thread and turn"
        )
    stamp = stop_events[0].get("ts") or stop_events[0].get("created_at")
    if not isinstance(stamp, str):
        raise RuntimeError("malformed-output: exact Stop receipt has no timestamp")
    try:
        stop_at = dt.datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()
    except ValueError as error:
        raise RuntimeError("malformed-output: exact Stop receipt timestamp is invalid") from error
    if nonce.stat().st_mtime <= stop_at:
        raise RuntimeError("parser-rejected: useful nonce action did not follow the exact Stop block")

    raise RuntimeError(
        "parser-rejected: the journey measured one independent Stop and nonce action, "
        "but did not collect native goal, effective-window, quiet-park, wake, and "
        "repeated compaction/resume receipts; no verified receipt was written"
    )


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
