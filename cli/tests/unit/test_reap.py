"""Tests for reap-dead-workers subcommand."""
from __future__ import annotations

import json
from datetime import datetime, timezone, timedelta
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


def _ts(minutes_ago: int) -> str:
    """ISO timestamp N minutes in the past."""
    return (datetime.now(timezone.utc) - timedelta(minutes=minutes_ago)).isoformat()


def _write_workers(workers_file: Path, workers: list[dict]) -> None:
    """Write a list of worker dicts to workers.jsonl."""
    with open(workers_file, "w") as f:
        for w in workers:
            f.write(json.dumps(w) + "\n")


def _fixture_three_workers(tmp_path: Path) -> tuple[Path, Path, str, str, str]:
    """Create the three-worker fixture from the plan spec.

    Returns: (workers_file, artifacts_dir, active_id, abandoned_id, completed_id)
    """
    workers_file = tmp_path / "workers.jsonl"
    artifacts_dir = tmp_path / "artifacts"
    artifacts_dir.mkdir()

    active_id = "worker-active-001"
    abandoned_id = "worker-abandoned-002"
    completed_id = "worker-completed-003"

    # Active: status=started, heartbeat 5min ago, no ship artifact
    active = {
        "worker_id": active_id,
        "task": "active task",
        "status": "started",
        "started_at": _ts(10),
        "last_heartbeat": _ts(5),
        "session_id": "sess-active",
    }

    # Abandoned: status=started, heartbeat 45min ago, no ship artifact
    abandoned = {
        "worker_id": abandoned_id,
        "task": "abandoned task",
        "status": "started",
        "started_at": _ts(50),
        "last_heartbeat": _ts(45),
        "session_id": "sess-abandoned",
    }

    # Completed: status=started, heartbeat 45min ago, ship artifact EXISTS
    completed_session = "sess-completed"
    ship_artifact = artifacts_dir / f"ship-{completed_session}.md"
    ship_artifact.write_text("# Ship completed\n")

    completed = {
        "worker_id": completed_id,
        "task": "completed task",
        "status": "started",
        "started_at": _ts(60),
        "last_heartbeat": _ts(45),
        "session_id": completed_session,
    }

    _write_workers(workers_file, [active, abandoned, completed])

    return workers_file, artifacts_dir, active_id, abandoned_id, completed_id


# AC1-HP: reap marks abandoned workers without removing rows
def test_ac1_hp_reap_marks_abandoned(tmp_path):
    """reap flips abandoned worker to status=abandoned; active and completed untouched."""
    workers_file, artifacts_dir, active_id, abandoned_id, completed_id = _fixture_three_workers(tmp_path)

    from fno.runtime.reap import reap_dead_workers

    report = reap_dead_workers(
        workers_file=workers_file,
        artifacts_dir=artifacts_dir,
        dry_run=False,
    )

    assert report["reaped"] == 1
    assert report["active"] == 1
    assert report["completed"] == 1

    # Verify file state
    from fno.runtime.registry import read_workers
    entries = {e["worker_id"]: e for e in read_workers(workers_file=workers_file)}

    assert entries[active_id]["status"] == "started", "active should remain started"
    assert entries[abandoned_id]["status"] == "abandoned", "abandoned should be flipped"
    assert entries[completed_id]["status"] == "started", "completed should remain started (ship artifact present)"

    # All three rows still exist (no deletion)
    assert len(entries) == 3


def test_ac1_hp_reap_stdout_json(tmp_path):
    """reap --json reports {reaped, active, completed}."""
    workers_file, artifacts_dir, _, _, _ = _fixture_three_workers(tmp_path)

    result = runner.invoke(
        app,
        [
            "runtime", "reap-dead-workers",
            "--workers-file", str(workers_file),
            "--artifacts-dir", str(artifacts_dir),
            "--json",
        ],
    )

    assert result.exit_code == 0, f"Output: {result.output}"
    data = json.loads(result.output)
    assert data["reaped"] == 1
    assert data["active"] == 1
    assert data["completed"] == 1


# AC2-HP: --dry-run reports without mutation
def test_ac2_hp_dry_run_no_mutation(tmp_path):
    """--dry-run produces the same report but does NOT change the file."""
    workers_file, artifacts_dir, active_id, abandoned_id, completed_id = _fixture_three_workers(tmp_path)

    original_content = workers_file.read_text()

    from fno.runtime.reap import reap_dead_workers

    report = reap_dead_workers(
        workers_file=workers_file,
        artifacts_dir=artifacts_dir,
        dry_run=True,
    )

    assert report["reaped"] == 1
    assert report["active"] == 1
    assert report["completed"] == 1

    # File must be unchanged
    assert workers_file.read_text() == original_content


def test_ac2_hp_dry_run_cli(tmp_path):
    """fno runtime reap-dead-workers --dry-run produces report without mutating file."""
    workers_file, artifacts_dir, _, _, abandoned_id = _fixture_three_workers(tmp_path)
    original = workers_file.read_text()

    result = runner.invoke(
        app,
        [
            "runtime", "reap-dead-workers",
            "--workers-file", str(workers_file),
            "--artifacts-dir", str(artifacts_dir),
            "--dry-run",
            "--json",
        ],
    )

    assert result.exit_code == 0, f"Output: {result.output}"
    data = json.loads(result.output)
    assert data["reaped"] == 1
    assert data.get("dry_run") is True

    # File unchanged
    assert workers_file.read_text() == original


def test_ac1_hp_empty_registry_no_crash(tmp_path):
    """reap on an empty workers.jsonl returns zeros without error."""
    workers_file = tmp_path / "workers.jsonl"
    workers_file.write_text("")
    artifacts_dir = tmp_path / "artifacts"
    artifacts_dir.mkdir()

    from fno.runtime.reap import reap_dead_workers

    report = reap_dead_workers(workers_file=workers_file, artifacts_dir=artifacts_dir)
    assert report["reaped"] == 0
    assert report["active"] == 0
    assert report["completed"] == 0


def test_ac1_hp_missing_registry_no_crash(tmp_path):
    """reap on a missing workers.jsonl returns zeros without error."""
    workers_file = tmp_path / "nonexistent.jsonl"
    artifacts_dir = tmp_path / "artifacts"
    artifacts_dir.mkdir()

    from fno.runtime.reap import reap_dead_workers

    report = reap_dead_workers(workers_file=workers_file, artifacts_dir=artifacts_dir)
    assert report["reaped"] == 0
