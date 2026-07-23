"""Tests for worker registry (workers.jsonl) and spawn/register-worker subcommands."""
from __future__ import annotations

import json
import multiprocessing
import os
import threading
import time
import uuid
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


# Module-top-level helper for multiprocessing.Pool. Closures and methods
# can't be pickled by Pool's worker spawn, so the callable lives here.
# Reference pattern: cli/tests/unit/test_event_log.py::_worker_emit.
def _register_worker_in_pool(args: tuple) -> None:
    """Worker function for cross-process race test.

    Args are passed as a tuple so Pool.map can fan out a single iterable.
    """
    from fno.runtime.registry import register_worker

    workers_file_str, worker_id, task, campaign = args
    register_worker(
        worker_id=worker_id,
        task=task,
        campaign=campaign,
        workers_file=Path(workers_file_str),
    )


# ---------------------------------------------------------------------------
# registry module tests
# ---------------------------------------------------------------------------

def test_ac2_hp_register_worker_appends_entry(tmp_path):
    """register_worker appends a JSONL entry to workers.jsonl."""
    from fno.runtime.registry import register_worker

    workers_file = tmp_path / "workers.jsonl"
    worker_id = str(uuid.uuid4())

    register_worker(
        worker_id=worker_id,
        task="build feature X",
        campaign="campaign-1",
        workers_file=workers_file,
    )

    assert workers_file.exists()
    lines = workers_file.read_text().strip().splitlines()
    assert len(lines) == 1
    entry = json.loads(lines[0])
    assert entry["worker_id"] == worker_id
    assert entry["task"] == "build feature X"
    assert entry["campaign"] == "campaign-1"
    assert entry["status"] == "started"
    assert "started_at" in entry


def test_ac2_hp_register_worker_atomic_concurrent(tmp_path):
    """Concurrent register_worker calls across PROCESSES produce N complete JSONL lines.

    Previously this used threading.Thread, which is serialized by the GIL
    for bytecode and never actually exercised the filelock's cross-process
    contention path. Port to multiprocessing.Pool so the workers run in
    real subprocesses and the FileLock in register_worker is the only
    thing preventing byte-level interleave on workers.jsonl. (#22)
    """
    workers_file = tmp_path / "workers.jsonl"
    n_workers = 4
    args_list = [
        (str(workers_file), f"worker-{i}", f"task-{i}", "campaign-1")
        for i in range(n_workers)
    ]

    with multiprocessing.Pool(processes=n_workers) as pool:
        pool.map(_register_worker_in_pool, args_list)

    lines = workers_file.read_text().strip().splitlines()
    assert len(lines) == n_workers, (
        f"Expected {n_workers} lines, got {len(lines)} - root cause could be "
        f"byte-level interleave (filelock failed) OR a worker that skipped "
        f"acquisition and silently returned without writing. Both are "
        f"failure modes the filelock is supposed to prevent."
    )
    ids = {json.loads(l)["worker_id"] for l in lines}
    assert ids == {f"worker-{i}" for i in range(n_workers)}
    # Each line must be valid JSON (no interleaved bytes).
    for line in lines:
        json.loads(line)


def test_ac2_hp_register_worker_multiple_appends(tmp_path):
    """register_worker appends to existing file rather than overwriting."""
    from fno.runtime.registry import register_worker

    workers_file = tmp_path / "workers.jsonl"
    for i in range(3):
        register_worker(
            worker_id=f"w-{i}",
            task=f"task-{i}",
            campaign="c1",
            workers_file=workers_file,
        )

    lines = workers_file.read_text().strip().splitlines()
    assert len(lines) == 3


# ---------------------------------------------------------------------------
# spawn subcommand tests
# ---------------------------------------------------------------------------

def test_ac1_hp_spawn_in_session_returns_skill_dispatch(monkeypatch, tmp_path):
    """spawn in-session returns skill_dispatch_required without registering worker."""
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "sess-abc123")
    workers_file = tmp_path / "workers.jsonl"

    result = runner.invoke(
        app,
        [
            "runtime", "spawn",
            "--prompt", "do something",
            "--adapter", "claude-code",
            "--workers-file", str(workers_file),
            "--json",
        ],
    )

    assert result.exit_code == 0, f"Output: {result.output}"
    data = json.loads(result.output)
    assert data["action"] == "skill_dispatch_required"
    # Must NOT have registered a worker
    assert not workers_file.exists()


def test_ac1_hp_spawn_external_registers_worker(monkeypatch, tmp_path):
    """spawn external spawns subprocess and registers worker in workers.jsonl."""
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    workers_file = tmp_path / "workers.jsonl"

    fake_proc = MagicMock()
    fake_proc.pid = 54321
    fake_proc.poll.return_value = None  # simulate still-running process

    with patch("subprocess.Popen", return_value=fake_proc):
        result = runner.invoke(
            app,
            [
                "runtime", "spawn",
                "--prompt", "build feature Y",
                "--adapter", "claude-code",
                "--workers-file", str(workers_file),
                "--json",
            ],
        )

    assert result.exit_code == 0, f"Output: {result.output}"
    data = json.loads(result.output)
    assert "worker_id" in data
    assert data["pid"] == 54321

    # Worker must be registered
    assert workers_file.exists()
    lines = workers_file.read_text().strip().splitlines()
    assert len(lines) == 1
    entry = json.loads(lines[0])
    assert entry["worker_id"] == data["worker_id"]
    assert entry["status"] == "started"


# ---------------------------------------------------------------------------
# register-worker subcommand (CLI)
# ---------------------------------------------------------------------------

def test_register_worker_cli(tmp_path):
    """fno runtime register-worker --id X appends to workers.jsonl."""
    workers_file = tmp_path / "workers.jsonl"
    worker_id = str(uuid.uuid4())

    result = runner.invoke(
        app,
        [
            "runtime", "register-worker",
            "--id", worker_id,
            "--task", "some task",
            "--workers-file", str(workers_file),
            "--json",
        ],
    )

    assert result.exit_code == 0, f"Output: {result.output}"
    data = json.loads(result.output)
    assert data["status"] == "registered"
    assert data["worker_id"] == worker_id

    lines = workers_file.read_text().strip().splitlines()
    assert len(lines) == 1
    assert json.loads(lines[0])["worker_id"] == worker_id
