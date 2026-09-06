"""notify_watch arm (x-5f06): sample computed signals, notify the operator on
a state change.

The pass runs beside king_wake on the pr-watch launchd cadence. It reads what
already exists (the king board, the court, main CI's check runs) and collapses
each signal on its token through ``fno.notify._signal`` - the token IS the
state, the tick is only a sample. A notification is a POINTER to the durable
queue (``fno inbox outstanding``, ``fno inbox board``), never a copy of it.
"""
from __future__ import annotations

import hashlib
import json
import logging
import shutil
import subprocess
from pathlib import Path
from typing import Any, Optional
from urllib.parse import quote

_log = logging.getLogger(__name__)

#: Board queues the arm subscribes to: key -> (pointer verb, count label).
BOARD_QUEUES = {
    "operator_question": ("fno inbox outstanding", "open operator question(s)"),
    "mergeable_pr": ("fno inbox board", "mergeable PR(s) with no live driver"),
    "undriven_pr": ("fno inbox board", "PR(s) undriven across checks"),
}

_CHECK_BAD = {"failure", "timed_out", "startup_failure", "action_required"}
_CHECK_GOOD = {"success", "neutral", "skipped"}


def _token(payload: Any) -> str:
    return hashlib.sha1(
        json.dumps(payload, sort_keys=True, ensure_ascii=True).encode()
    ).hexdigest()


def _rows_token(rows: list) -> str:
    ids = sorted(
        str(r.get("id") or r.get("number")) for r in rows if isinstance(r, dict)
    )
    return _token(ids)


def _run(cmd: list[str], timeout: int = 60) -> Optional[str]:
    try:
        proc = subprocess.run(
            cmd, capture_output=True, text=True, check=False, timeout=timeout
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        _log.warning("notify_watch: %s failed: %s", cmd[0], exc)
        return None
    return proc.stdout or ""


def _board() -> Optional[dict]:
    """The king board payload, read through the same Rust binary the verb uses."""
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        return None
    try:
        return json.loads(_run([str(binary), "board", "--json"]) or "")
    except ValueError:
        return None


def _crown_token() -> tuple[Optional[str], int]:
    """(token, live crown count); (None, 0) when the registry is unreadable."""
    from fno.agents.court import gather_court

    court = gather_court()
    crowns = court.get("crowns")
    if crowns is None:
        return None, 0
    triples = sorted(
        f"{c.get('scope')}:{c.get('holder')}:{c.get('status')}" for c in crowns
    )
    return _token(triples), len(triples)


def _default_branch(repo_dir: Path) -> str:
    out = _run(
        ["git", "symbolic-ref", "refs/remotes/origin/HEAD", "--short"], timeout=10
    )
    branch = (out or "").strip()
    return branch.split("/", 1)[1] if "/" in branch else "main"


def _main_ci_sample(repo_dir: Path) -> Optional[dict]:
    """One repo's main-CI sample, or None when it cannot be read.

    A sample with no terminal verdict yet (runs still pending) is also None:
    an in-flight run is not a state, and notifying on it would spam.
    """
    out = _run(["git", "remote", "get-url", "origin"], cwd=str(repo_dir), timeout=10)
    url = (out or "").strip()
    if ":" in url:
        slug = url.split(":", 1)[1].removesuffix(".git")
    else:
        slug = url.removesuffix(".git").rsplit("/", 1)[-1] if url else ""
    slug = slug.strip("/")
    if "/" not in slug:
        return None
    owner_repo = slug
    branch = _default_branch(repo_dir)
    out = _run(
        [
            "gh",
            "api",
            f"repos/{owner_repo}/commits/{quote(branch, safe='')}/check-runs",
        ],
        timeout=30,
    )
    if out is None:
        return None
    try:
        runs = (json.loads(out).get("check_runs")) or []
    except ValueError:
        return None
    conclusions = [r.get("conclusion") for r in runs if isinstance(r, dict)]
    if not conclusions:
        return None
    if any(c in _CHECK_BAD for c in conclusions):
        verdict = "failure"
    elif all(c in _CHECK_GOOD for c in conclusions):
        verdict = "success"
    else:
        return None
    return {
        "key": f"main_ci:{owner_repo}",
        "label": owner_repo,
        "token": verdict,
        "body": f"main CI on {owner_repo}: {verdict}.",
        "pointer": f"https://github.com/{owner_repo}/actions",
    }


def _send(_signal, key: str, token: str, title: str, body: str, pointer: str) -> str:
    """One collapsed send; returns the verdict for the tick detail."""
    code, verdict = _signal.notify_signal(key, token, title, body, pointer)
    return "sent" if code == 0 and verdict == "sent" else verdict


def run_notify_watch(
    settings: Any,
    signals: list[str],
    roots: Optional[list[Path]] = None,
) -> tuple[int, Optional[str], str]:
    """One pass. Returns ``(acted, skip_reason, detail)`` for the tick row.

    ``acted`` counts notifications that left the machine. ``skip_reason`` names
    only whole-lane trouble (gh absent for main_ci, the board unreadable);
    dedupe is the designed quiet, not a skip.
    """
    from fno.notify import _signal

    acted = 0
    notes: list[str] = []
    skip: Optional[str] = None

    board_keys = [s for s in signals if s in BOARD_QUEUES]
    queues: dict[str, dict] = {}
    if board_keys:
        board = _board()
        if board is None:
            skip = "board_unreadable"
            notes.append("board:unreadable")
        else:
            queues = {
                q.get("name"): q
                for q in board.get("queues", [])
                if isinstance(q, dict)
            }

    for key in ("operator_question", "mergeable_pr", "undriven_pr"):
        if key not in signals:
            continue
        queue = queues.get(key)
        if queue is None:
            if board_keys:
                notes.append(f"{key}:queue_absent")
            continue
        pointer, label = BOARD_QUEUES[key]
        count = int(queue.get("count") or 0)
        if count == 0:
            _signal.forget(key)  # drained: forget so a regrowth notifies
            notes.append(f"{key}:clear")
            continue
        token = _rows_token(queue.get("rows") or [])
        verdict = _send(_signal, key, token, f"operator: {label}",
                        f"{count} {label}. {pointer}", pointer)
        acted += verdict == "sent"
        notes.append(f"{key}:{verdict}")

    if "crown_set" in signals:
        token, crowns = _crown_token()
        if token is None:
            notes.append("crown_set:unreadable")
        elif crowns == 0:
            _signal.forget("crown_set")
            notes.append("crown_set:clear")
        else:
            verdict = _send(
                _signal, "crown_set", token, "court changed",
                f"{crowns} crown(s) live on the court. fno agents court",
                "fno agents court",
            )
            acted += verdict == "sent"
            notes.append(f"crown_set:{verdict}")

    if "main_ci" in signals:
        if shutil.which("gh") is None:
            skip = skip or "gh_absent"
            notes.append("main_ci:gh_absent")
        else:
            for root in roots or []:
                sample = _main_ci_sample(root)
                if sample is None:
                    notes.append(f"main_ci:{root.name}:unreadable")
                    continue
                verdict = _send(
                    _signal, sample["key"], sample["token"],
                    "main CI", sample["body"], sample["pointer"],
                )
                acted += verdict == "sent"
                notes.append(f"{sample['key']}:{verdict}")

    return acted, skip, "; ".join(notes)[:200]
