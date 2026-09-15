"""x-385e change 6e: two concurrent `fno backlog idea` filings both persist.

End-to-end port of the incident that filed this node: two idea receipts
printed exit-0, one node never existed. With the fix, every receipt's node
resolves. A third child loops `backlog note` to keep the file churning.
Optional (skips without the Rust worker, like the parity suite). It never
reads or writes ~/.fno: every child gets HOME and FNO_HOME pointed at tmp.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
from pathlib import Path

import pytest

from fno.graph.store import _worker_binary

# The store spawns keepers from fno-agents-worker; find_dev_binary() answers
# for the fno-agents daemon and is the WRONG pin here (a daemon given
# --store-keeper exits 2).
requires_rust = pytest.mark.skipif(
    _worker_binary() is None,
    reason=(
        "compiled fno-agents-worker binary not present "
        "(build with `cargo build -p fno-agents --bin fno-agents-worker`)"
    ),
)


def _child_env(tmp_path: Path, worker: str) -> dict:
    env = dict(os.environ)
    env["HOME"] = str(tmp_path)
    env["FNO_HOME"] = str(tmp_path)
    env["FNO_AGENTS_WORKER"] = worker
    return env


def _run_backlog(env: dict, *args: str, timeout: float = 300.0) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "-c", "from fno.cli import app; app()", *args],
        env=env,
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def test_two_concurrent_idea_filings_both_persist(tmp_path):
    worker = _worker_binary()
    assert worker is not None
    env = _child_env(tmp_path, str(worker))

    # Seed the graph so the note loop has a target and every publish is a
    # real whole-file write.
    seeded = _run_backlog(env, "backlog", "idea", "seed row", "--difficulty", "low")
    assert seeded.returncode == 0, seeded.stderr
    seed_receipt = json.loads(seeded.stdout)
    seed_id = seed_receipt["id"]

    receipts: list[dict] = []
    errors: list[str] = []
    start = threading.Barrier(3)

    def file_idea(title: str) -> None:
        start.wait()
        run = _run_backlog(
            env, "backlog", "idea", title, "--difficulty", "low", "--separate"
        )
        if run.returncode != 0:
            errors.append(f"idea {title!r} exited {run.returncode}: {run.stderr}")
            return
        try:
            receipts.append(json.loads(run.stdout))
        except json.JSONDecodeError as e:
            errors.append(f"idea {title!r} printed no JSON receipt: {run.stdout!r} ({e})")

    def churn_notes() -> None:
        start.wait()
        for i in range(25):
            note = _run_backlog(
                env,
                "backlog",
                "note",
                seed_id,
                f"churn note {i}",
                "--quiet",
                "--json",
                timeout=120.0,
            )
            if note.returncode != 0:
                # x-385e makes a lost race exit 1: a churning note may fail
                # loud, but it must never claim success while losing the row.
                continue
        return

    threads = [
        threading.Thread(target=file_idea, args=("concurrent filing one",)),
        threading.Thread(target=file_idea, args=("concurrent filing two",)),
        threading.Thread(target=churn_notes),
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=600)

    assert not errors, f"idea children failed: {errors}"
    assert len(receipts) == 2, f"expected two receipts, got {receipts}"

    # THE CONTRACT: each receipt's node resolves in the same env.
    for receipt in receipts:
        got = _run_backlog(env, "backlog", "get", receipt["id"])
        assert got.returncode == 0, (
            f"idea receipt claimed {receipt['id']} but backlog get failed: {got.stderr}"
        )
