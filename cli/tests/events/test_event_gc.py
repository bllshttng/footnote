from __future__ import annotations

import json
import os
import subprocess
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

import pytest
from typer.testing import CliRunner

import fno.events.gc as event_gc

from fno.events import (
    EVENT_TYPES,
    SCHEMA,
    SchemaUnavailableError,
    retention_for,
    validate_retention_schema,
)
from fno.events.gc import gc_events
from fno.events.cli import cli as event_cli


NOW = datetime(2026, 8, 11, 12, 0, tzinfo=timezone.utc)


def _event(event_type: str, ts: str) -> str:
    source = "test"
    data: dict[str, object] = {}
    if event_type == "claim_acquired":
        source = "fno-loop"
        data = {
            "key": "node:test",
            "holder": "test",
            "pid": 1,
            "host": "test",
            "acquired_at": 1,
        }
    elif event_type == "claim_released":
        source = "fno-loop"
        data = {
            "key": "node:test",
            "holder": "test",
            "pid": 1,
            "host": "test",
            "acquired_at": 1,
            "duration_held_ms": 1,
        }
    elif event_type == "human_touch":
        data = {"graph_node_id": "test", "source": "answer", "resolution": "ok"}
    return json.dumps({"ts": ts, "type": event_type, "source": source, "data": data})


def test_schema_declares_measured_retention_classes() -> None:
    assert EVENT_TYPES is not None
    assert EVENT_TYPES["event_migration_landed"]["retention"] == "durable"
    assert retention_for("claim_acquired") == "ephemeral"
    assert retention_for("human_touch") == "ephemeral"
    assert retention_for("review_attestation") == "gate"
    assert retention_for("review_coverage") == "gate"
    assert retention_for("agent_spawned") == "durable"
    assert retention_for("node_closed") == "durable"
    assert retention_for("advance_dispatched") == "durable"
    assert retention_for("think_spawned") == "durable"


def test_undeclared_retention_fails_closed_to_durable() -> None:
    assert retention_for("event_payload_too_large") == "durable"
    assert retention_for("unknown_future_type") == "durable"


def test_join_pair_cannot_straddle_retention_classes() -> None:
    schema = {
        "retention": {"default": "durable", "joins": [["spawn", "contact"]]},
        "event_types": [
            {"name": "spawn", "retention": "durable"},
            {"name": "contact", "retention": "ephemeral"},
        ],
    }
    with pytest.raises(SchemaUnavailableError, match="retention join mismatch"):
        validate_retention_schema(schema)


def test_gc_deletes_only_expired_explicit_ephemeral_rows(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    rows = [
        _event("claim_acquired", "2026-07-01T00:00:00Z"),
        _event("human_touch", "2026-08-10T00:00:00Z"),
        _event("review_attestation", "2026-07-01T00:00:00Z"),
        _event("agent_spawned", "2026-07-01T00:00:00Z"),
        _event("event_payload_too_large", "2026-07-01T00:00:00Z"),
        _event("unknown_future_type", "2026-07-01T00:00:00Z"),
        "not-json",
    ]
    events.write_text("\n".join(rows) + "\n", encoding="utf-8")

    result = gc_events(events, now=NOW, ttl_hours=672)

    kept = events.read_text(encoding="utf-8").splitlines()
    assert result == {"scanned": 7, "deleted": 1, "kept": 6, "malformed": 1}
    assert not any('"type": "claim_acquired"' in row for row in kept)
    assert any('"type": "human_touch"' in row for row in kept)
    assert any('"type": "review_attestation"' in row for row in kept)
    assert any('"type": "agent_spawned"' in row for row in kept)
    assert any('"type": "event_payload_too_large"' in row for row in kept)
    assert any('"type": "unknown_future_type"' in row for row in kept)
    assert "not-json" in kept


def test_gc_refuses_a_horizon_shorter_than_schema_minimum(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    events.write_text(_event("claim_acquired", "2026-07-01T00:00:00Z") + "\n")

    with pytest.raises(ValueError, match="minimum retention horizon"):
        gc_events(events, now=NOW, ttl_hours=671)


def test_gc_refuses_the_unlocked_global_daemon_journal(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    events = tmp_path / "global-events.jsonl"
    events.write_text(
        _event("claim_acquired", "2026-07-01T00:00:00Z") + "\n",
        encoding="utf-8",
    )
    monkeypatch.setattr("fno.paths.global_events_json", lambda: events)

    with pytest.raises(ValueError, match="global daemon journal"):
        gc_events(events, now=NOW, ttl_hours=672)

    assert events.read_text(encoding="utf-8")


def test_gc_dry_run_reports_without_rewriting(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    original = _event("claim_acquired", "2026-07-01T00:00:00Z") + "\n"
    events.write_text(original, encoding="utf-8")

    result = gc_events(events, now=NOW, ttl_hours=672, dry_run=True)

    assert result == {"scanned": 1, "deleted": 1, "kept": 0, "malformed": 0}
    assert events.read_text(encoding="utf-8") == original


def test_real_schema_retention_joins_validate() -> None:
    assert SCHEMA is not None
    validate_retention_schema(SCHEMA)


def test_gc_preserves_symlink_and_compacts_its_target(tmp_path: Path) -> None:
    target = tmp_path / "repo-events.jsonl"
    target.write_text(
        _event("claim_released", "2026-07-01T00:00:00Z")
        + "\n"
        + _event("node_closed", "2026-07-01T00:00:00Z")
        + "\n",
        encoding="utf-8",
    )
    link = tmp_path / "worktree-events.jsonl"
    link.symlink_to(target)

    gc_events(link, now=NOW, ttl_hours=672)

    assert link.is_symlink()
    assert '"type": "claim_released"' not in target.read_text(encoding="utf-8")
    assert '"type": "node_closed"' in target.read_text(encoding="utf-8")


def test_gc_retries_when_setup_retargets_leaf_while_waiting(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    local = tmp_path / "worktree-events.jsonl"
    local.touch()
    canonical = tmp_path / "canonical-events.jsonl"
    canonical.write_text(
        _event("claim_released", "2026-07-01T00:00:00Z")
        + "\n"
        + _event("node_closed", "2026-07-01T00:00:00Z")
        + "\n",
        encoding="utf-8",
    )
    acquired: list[Path] = []

    def acquire(lock_dir: Path, timeout_seconds: float) -> str:
        acquired.append(lock_dir)
        if len(acquired) == 1:
            local.unlink()
            local.symlink_to(canonical)
        return f"token-{len(acquired)}"

    monkeypatch.setattr(event_gc, "acquire_dir_mutex", acquire)
    monkeypatch.setattr(event_gc, "release_dir_mutex", lambda *_: None)
    monkeypatch.setattr(event_gc, "renew_dir_mutex", lambda *_: True)

    event_gc.gc_events(local, now=NOW, ttl_hours=672)

    gc_locks = [path for path in acquired if path.name.endswith(".gc.d")]
    assert gc_locks == [
        tmp_path / "worktree-events.jsonl.gc.d",
        tmp_path / "canonical-events.jsonl.gc.d",
    ]
    assert local.is_symlink()
    assert '"type": "claim_released"' not in canonical.read_text(encoding="utf-8")
    assert '"type": "node_closed"' in canonical.read_text(encoding="utf-8")


def test_gc_remaps_offer_cursor_without_skipping_pending_rows(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    consumed = _event("node_closed", "2026-07-01T00:00:00Z") + "\n"
    expired = _event("claim_acquired", "2026-07-01T00:00:00Z") + "\n"
    pending = _event("think_offered", "2026-08-11T00:00:00Z") + "\n"
    tail = _event("node_closed", "2026-08-11T00:01:00Z") + "\n"
    events.write_text(consumed + expired + pending + tail, encoding="utf-8")
    cursor = tmp_path / ".think-offer-cursor"
    cursor.write_text(str(len((consumed + expired).encode())), encoding="ascii")

    gc_events(events, now=NOW, ttl_hours=672)

    remapped = int(cursor.read_text(encoding="ascii"))
    assert remapped == len(consumed.encode())
    assert b"think_offered" in events.read_bytes()[remapped:]
    assert not cursor.with_name(cursor.name + ".gc-pending").exists()


def test_gc_preserves_rows_that_anchor_a_fanout_occurrence_cursor(tmp_path: Path) -> None:
    from fno import status_fanout
    from fno.config import StatusSinkConfig

    state_dir = tmp_path / ".fno"
    status_dir = state_dir / "status-sinks"
    status_dir.mkdir(parents=True)
    events = state_dir / "events.jsonl"
    cursor_ts = "2026-07-01T00:00:00Z"
    older = _event("claim_acquired", "2026-06-30T00:00:00Z") + "\n"
    anchor = _event("claim_acquired", cursor_ts) + "\n"
    delivered = json.dumps(
        {"ts": cursor_ts, "type": "blocked", "source": "target", "data": {}}
    ) + "\n"
    pending = json.dumps(
        {
            "ts": cursor_ts,
            "type": "blocked",
            "source": "target",
            "data": {"reason": "pending"},
        }
    ) + "\n"
    events.write_text(older + anchor + delivered + pending, encoding="utf-8")
    (status_dir / "s.cursor").write_text(
        json.dumps({"ts": cursor_ts, "n": 2}), encoding="utf-8"
    )

    result = gc_events(events, now=NOW, ttl_hours=672)

    assert result["deleted"] == 1
    assert events.read_text(encoding="utf-8").count('"type": "claim_acquired"') == 1
    dispatched: list[str] = []
    sink = StatusSinkConfig(
        name="s", type="json-webhook", events=["blocked"], url="https://x"
    )
    status_fanout.run_tick(
        tmp_path,
        [sink],
        dispatch_fn=lambda _sink, event: (
            dispatched.append(str(event["data"].get("reason", "")))
            or (status_fanout.DELIVERED, "")
        ),
    )
    assert dispatched == ["pending"]


def test_gc_recovers_cursor_mapping_left_by_interrupted_migration(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    row = _event("node_closed", "2026-08-11T00:00:00Z") + "\n"
    events.write_text(row, encoding="utf-8")
    cursor = tmp_path / ".think-offer-cursor"
    cursor.write_text("0", encoding="ascii")
    stat = events.stat()
    cursor.with_name(cursor.name + ".gc-pending").write_text(
        json.dumps({"device": stat.st_dev, "inode": stat.st_ino, "cursor": len(row)}),
        encoding="ascii",
    )

    gc_events(events, now=NOW, ttl_hours=672, dry_run=True)

    assert cursor.read_text(encoding="ascii") == str(len(row))
    assert not cursor.with_name(cursor.name + ".gc-pending").exists()


def test_gc_preserves_malformed_utf8_bytes(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    malformed = b'{"type":"note","data":{"text":"caf\xc3"}}\n'
    expired = (_event("claim_released", "2026-07-01T00:00:00Z") + "\n").encode()
    events.write_bytes(malformed + expired)

    result = gc_events(events, now=NOW, ttl_hours=672)

    assert result == {"scanned": 2, "deleted": 1, "kept": 1, "malformed": 1}
    assert events.read_bytes() == malformed


def test_gc_preserves_ephemeral_row_with_noncanonical_timestamp(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    row = _event("claim_released", "2026-07-01 00:00:00+00:00") + "\n"
    events.write_text(row, encoding="utf-8")

    result = gc_events(events, now=NOW, ttl_hours=672)

    assert result == {"scanned": 1, "deleted": 0, "kept": 1, "malformed": 1}
    assert events.read_text(encoding="utf-8") == row


def test_gc_preserves_schema_invalid_expired_ephemeral_row(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    row = json.dumps(
        {
            "ts": "2026-07-01T00:00:00Z",
            "type": "claim_acquired",
            "source": "fno-loop",
            "data": {},
        }
    ) + "\n"
    events.write_text(row, encoding="utf-8")

    result = gc_events(events, now=NOW, ttl_hours=672)

    assert result == {"scanned": 1, "deleted": 0, "kept": 1, "malformed": 1}
    assert events.read_text(encoding="utf-8") == row


def test_gc_does_not_fallback_when_canonical_timestamp_is_present_but_invalid(
    tmp_path: Path,
) -> None:
    events = tmp_path / "events.jsonl"
    row = (
        json.dumps(
            {
                "ts": "",
                "timestamp": "2026-07-01T00:00:00Z",
                "type": "claim_released",
                "source": "test",
                "data": {},
            }
        )
        + "\n"
    )
    events.write_text(row, encoding="utf-8")

    result = gc_events(events, now=NOW, ttl_hours=672)

    assert result == {"scanned": 1, "deleted": 0, "kept": 1, "malformed": 1}
    assert events.read_text(encoding="utf-8") == row


def test_gc_waits_for_registered_shell_writer(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    events.write_text(
        _event("claim_released", "2026-07-01T00:00:00Z") + "\n",
        encoding="utf-8",
    )
    active = tmp_path / "events.jsonl.shell-writers.d" / "writer"
    active.mkdir(parents=True)
    result: dict[str, int] = {}

    def collect() -> None:
        result.update(gc_events(events, now=NOW, ttl_hours=672))

    thread = threading.Thread(target=collect)
    thread.start()
    time.sleep(0.2)
    assert thread.is_alive()
    assert events.read_text(encoding="utf-8")

    active.rmdir()
    active.parent.rmdir()
    thread.join(timeout=5)

    assert not thread.is_alive()
    assert result["deleted"] == 1
    assert events.read_text(encoding="utf-8") == ""


def test_gc_reaps_reused_pid_writer_token_by_process_identity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    events = tmp_path / "events.jsonl"
    events.touch()
    token = tmp_path / "events.jsonl.shell-writers.d" / f"{os.getpid()}.writer"
    token.mkdir(parents=True)
    (token / "owner").write_text("original-process", encoding="utf-8")
    monkeypatch.setattr(event_gc, "_process_identity", lambda pid: "reused-process", raising=False)

    event_gc._wait_for_shell_writers(events, 0.2)

    assert not token.exists()


def test_process_identity_uses_shell_writer_locale(monkeypatch: pytest.MonkeyPatch) -> None:
    observed: dict[str, object] = {}

    def run(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
        observed.update(kwargs)
        return subprocess.CompletedProcess(args, 0, stdout="Tue Aug 11 09:13:37 2026\n")

    monkeypatch.setattr(event_gc.subprocess, "run", run)

    assert event_gc._process_identity(os.getpid()) == "Tue Aug 11 09:13:37 2026"
    env = observed["env"]
    assert isinstance(env, dict)
    assert env["LC_ALL"] == "C"


def test_gc_keeps_live_writer_when_identity_probe_is_unavailable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    events = tmp_path / "events.jsonl"
    events.touch()
    token = tmp_path / "events.jsonl.shell-writers.d" / f"{os.getpid()}.writer"
    token.mkdir(parents=True)
    (token / "owner").write_text("live-process", encoding="utf-8")
    monkeypatch.setattr(event_gc, "_process_identity", lambda pid: None)

    with pytest.raises(TimeoutError, match="shell writer rendezvous timeout"):
        event_gc._wait_for_shell_writers(events, 0.05)

    assert token.exists()


def test_gc_reaps_dead_writer_when_identity_probe_is_empty(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    events = tmp_path / "events.jsonl"
    events.touch()
    token = tmp_path / "events.jsonl.shell-writers.d" / "12345.writer"
    token.mkdir(parents=True)
    (token / "owner").write_text("dead-process", encoding="utf-8")
    monkeypatch.setattr(event_gc, "_process_identity", lambda pid: None)

    def dead(pid: int, signal: int) -> None:
        raise ProcessLookupError

    monkeypatch.setattr(event_gc.os, "kill", dead)

    event_gc._wait_for_shell_writers(events, 0.2)

    assert not token.exists()


def test_gc_renews_both_long_held_mutex_leases(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    events = tmp_path / "events.jsonl"
    events.write_text(
        _event("claim_released", "2026-07-01T00:00:00Z") + "\n",
        encoding="utf-8",
    )
    renewed: list[str] = []

    def record_renewal(lock_dir: Path, token: str) -> bool:
        renewed.append(lock_dir.name)
        return True

    monkeypatch.setattr(event_gc, "_LEASE_RENEW_EVERY_S", 0)
    monkeypatch.setattr(event_gc, "renew_dir_mutex", record_renewal)

    event_gc.gc_events(events, now=NOW, ttl_hours=672)

    assert "events.jsonl.gc.d" in renewed
    assert "events.jsonl.lock.d" in renewed


def test_gc_verifies_every_mutex_immediately_before_replace(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    events = tmp_path / "events.jsonl"
    events.write_text(
        _event("claim_released", "2026-07-01T00:00:00Z") + "\n",
        encoding="utf-8",
    )
    replacements: list[tuple[Path, Path]] = []
    real_replace = event_gc.os.replace

    def lose_final_lease(lock_dir: Path, token: str) -> bool:
        return False

    def record_replace(source: Path, destination: Path) -> None:
        replacements.append((Path(source), Path(destination)))
        real_replace(source, destination)

    monkeypatch.setattr(event_gc, "renew_dir_mutex", lose_final_lease)
    monkeypatch.setattr(event_gc.os, "replace", record_replace)

    with pytest.raises(RuntimeError, match="mutex ownership was lost"):
        event_gc.gc_events(events, now=NOW, ttl_hours=672)

    assert replacements == []
    assert events.read_text(encoding="utf-8").count("claim_released") == 1


def test_gc_cli_reports_and_rejects_short_horizon(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    events.write_text(
        _event("claim_acquired", "2000-01-01T00:00:00Z")
        + "\n"
        + _event("agent_spawned", "2000-01-01T00:00:00Z")
        + "\n",
        encoding="utf-8",
    )
    runner = CliRunner()

    result = runner.invoke(event_cli, ["gc", "--events", str(events)])
    assert result.exit_code == 0, result.output
    assert "scanned=2 deleted=1 kept=1 malformed=0" in result.output

    result = runner.invoke(
        event_cli,
        ["gc", "--events", str(events), "--ttl-hours", "671"],
    )
    assert result.exit_code == 1
    assert "minimum retention horizon" in result.output


def test_gc_cli_reports_mutex_ownership_loss(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    events = tmp_path / "events.jsonl"
    events.touch()

    def lose_lease(*args: object, **kwargs: object) -> dict[str, int]:
        raise RuntimeError("events.jsonl GC mutex ownership was lost")

    monkeypatch.setattr(event_gc, "gc_events", lose_lease)

    result = CliRunner().invoke(event_cli, ["gc", "--events", str(events)])

    assert result.exit_code == 1
    assert result.exception is not None
    assert "error: event gc failed: events.jsonl GC mutex ownership was lost" in result.output
    assert "Traceback" not in result.output
