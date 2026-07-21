"""Tests for scripts/migrate-events-shape.py.

Covers:
  - migrates legacy {timestamp, source, type, data} -> canonical
    {ts, type, source, data}
  - idempotent on canonical-only files (byte-for-byte equal output)
  - mixed-shape files (only legacy rows rewritten)
  - corrupt JSONL rows preserved verbatim with sidecar log
  - lock contention aborts cleanly with rc=2 (mkdir-based mutex shared
    with fno.events.append_event so cross-language callers serialize)
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
SCRIPT = REPO_ROOT / "scripts/migrate-events-shape.py"


def _write_events(path: Path, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as fh:
        for row in rows:
            fh.write(json.dumps(row) + "\n")


def _read_events(path: Path) -> list[dict]:
    """Tolerant reader: skips blank and unparseable lines.

    Tests for the corrupt-preserved invariant rely on the migration script
    leaving bad lines in place; we don't want this helper to choke on the
    fragment when verifying the surrounding migrated rows.
    """
    out: list[dict] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return out


@pytest.fixture
def workdir(tmp_path):
    (tmp_path / ".fno").mkdir()
    return tmp_path


def test_migrates_legacy_rows(workdir):
    target = workdir / ".fno/events.jsonl"
    legacy = [
        {
            "timestamp": "2026-05-07T09:30:42Z",
            "source": "target",
            "type": "phase_init",
            "data": {"phase": "register", "nonce": "n", "session_id": "s"},
        },
        {
            "timestamp": "2026-05-07T09:31:00Z",
            "source": "target",
            "type": "phase_transition",
            "data": {
                "gate_bearing": True,
                "gate": "ledger_updated",
                "phase": "register",
                "nonce": "n",
                "session_id": "s",
            },
        },
    ]
    _write_events(target, legacy)
    rc = subprocess.call([sys.executable, str(SCRIPT), "--root", str(workdir)])
    assert rc == 0
    out = _read_events(target)
    assert all({"ts", "type", "source", "data"} <= set(r.keys()) for r in out)
    assert all("timestamp" not in r for r in out)
    assert (workdir / ".fno/events.jsonl.bak").exists()


def test_idempotent_on_canonical_rows(workdir):
    target = workdir / ".fno/events.jsonl"
    canonical = [
        {
            "ts": "2026-05-07T09:30:42Z",
            "type": "phase_init",
            "source": "target",
            "data": {"phase": "register", "nonce": "n", "session_id": "s"},
        }
    ]
    _write_events(target, canonical)
    before = target.read_bytes()
    subprocess.call([sys.executable, str(SCRIPT), "--root", str(workdir)])
    after = target.read_bytes()
    assert before == after, "canonical-only file should not be modified"


def test_mixed_shape_file(workdir):
    target = workdir / ".fno/events.jsonl"
    rows = [
        {
            "timestamp": "2026-05-07T09:30:42Z",
            "source": "target",
            "type": "phase_init",
            "data": {"phase": "p", "nonce": "n", "session_id": "s"},
        },  # legacy
        {
            "ts": "2026-05-07T09:31:00Z",
            "type": "phase_init",
            "source": "target",
            "data": {"phase": "p", "nonce": "n", "session_id": "s"},
        },  # canonical
    ]
    _write_events(target, rows)
    rc = subprocess.call([sys.executable, str(SCRIPT), "--root", str(workdir)])
    assert rc == 0
    out = _read_events(target)
    assert all("timestamp" not in r for r in out)
    # Second run is a no-op (file already canonical).
    before = target.read_bytes()
    subprocess.call([sys.executable, str(SCRIPT), "--root", str(workdir)])
    after = target.read_bytes()
    assert before == after, "second run on already-migrated file produced changes"


def test_corrupt_row_preserved(workdir):
    target = workdir / ".fno/events.jsonl"
    target.write_text(
        '{"timestamp":"2026-05-07T09:30:42Z","source":"target","type":"phase_init","data":{"phase":"p","nonce":"n","session_id":"s"}}\n'
        '{"timestamp":"2026-05-07T09:30:43Z","source":"target","type":"phase\n'
        '{"timestamp":"2026-05-07T09:30:44Z","source":"target","type":"phase_init","data":{"phase":"p","nonce":"n","session_id":"s"}}\n',
        encoding="utf-8",
    )
    rc = subprocess.call([sys.executable, str(SCRIPT), "--root", str(workdir)])
    assert rc == 0
    corrupt_log = workdir / ".fno/events.jsonl.corrupt"
    assert corrupt_log.exists()
    log_text = corrupt_log.read_text(encoding="utf-8")
    assert "line 2" in log_text
    out = _read_events(target)
    # Row 1 and row 3 (rows[0] and rows[2]) are migrated; line 2 is a fragment
    # preserved verbatim. The output file still contains 3 lines: 2 migrated +
    # 1 fragment that does not parse cleanly via _read_events (which skips it).
    assert len(out) == 2
    assert all("timestamp" not in r for r in out)


def test_lock_timeout(workdir):
    """Hold the mkdir-based mutex from another process and verify migration
    aborts cleanly with rc=2 after the configured timeout.
    """
    target = workdir / ".fno/events.jsonl"
    _write_events(
        target,
        [
            {
                "timestamp": "2026-05-07T09:30:42Z",
                "source": "target",
                "type": "phase_init",
                "data": {"phase": "p", "nonce": "n", "session_id": "s"},
            }
        ],
    )

    lock_dir = target.parent / (target.name + ".lock.d")
    lock_dir.mkdir()  # simulate another process holding the mutex

    try:
        proc = subprocess.run(
            [sys.executable, str(SCRIPT), "--root", str(workdir)],
            env={**os.environ, "MIGRATE_LOCK_TIMEOUT_SECONDS": "1"},
            capture_output=True,
            text=True,
        )
        assert proc.returncode == 2, f"rc={proc.returncode} stderr={proc.stderr}"
        assert "session active, refused to race" in proc.stderr
        # File untouched.
        assert "timestamp" in target.read_text()
    finally:
        lock_dir.rmdir()


def test_dry_run_makes_no_changes(workdir):
    target = workdir / ".fno/events.jsonl"
    legacy = [
        {
            "timestamp": "2026-05-07T09:30:42Z",
            "source": "target",
            "type": "phase_init",
            "data": {"phase": "p", "nonce": "n", "session_id": "s"},
        }
    ]
    _write_events(target, legacy)
    before = target.read_bytes()
    rc = subprocess.call(
        [sys.executable, str(SCRIPT), "--root", str(workdir), "--dry-run"]
    )
    assert rc == 0
    after = target.read_bytes()
    assert before == after, "dry-run modified the file"
    assert not (workdir / ".fno/events.jsonl.bak").exists()


def test_long_migration_renews_its_lock_lease(tmp_path, monkeypatch):
    """A migration legitimately holds the events lock far longer than the
    steal threshold, so it must keep the lease fresh.

    Without renewal an ordinary emitter age-steals the live lock, appends to
    the inode the migration is about to replace, and those events are lost.
    """
    import importlib.util

    from fno.mutex import STALE_MUTEX_STEAL_S

    script = next(
        (
            c
            for p in Path(__file__).resolve().parents
            for c in [p / "scripts" / "migrate-events-shape.py"]
            if c.is_file()
        ),
        None,
    )
    assert script is not None, "migrate-events-shape.py not found"
    spec = importlib.util.spec_from_file_location("migrate_events_shape", script)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)

    events = tmp_path / "events.jsonl"
    events.write_text(
        "".join(
            json.dumps({"timestamp": "2026-01-01T00:00:00Z", "type": "t", "source": "s"})
            + "\n"
            for _ in range(50)
        ),
        encoding="utf-8",
    )
    lock_dir = tmp_path / "events.jsonl.lock.d"
    lock_dir.mkdir()
    # Backdate the lease so the very first renewal must fire.
    old = time.time() - (STALE_MUTEX_STEAL_S + 60)
    os.utime(lock_dir, (old, old))

    monkeypatch.setattr(mod, "_LEASE_RENEW_EVERY_S", 0)
    mod._do_migrate(events, dry_run=True, lock_dir=lock_dir)

    age = time.time() - lock_dir.lstat().st_mtime
    assert age < STALE_MUTEX_STEAL_S, (
        f"lease not renewed: lock still looks {int(age)}s old and is stealable "
        "mid-migration"
    )
