from __future__ import annotations

import json
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
    return json.dumps({"ts": ts, "type": event_type, "source": "test", "data": {}})


def test_schema_declares_measured_retention_classes() -> None:
    assert EVENT_TYPES is not None
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


def test_gc_preserves_malformed_utf8_bytes(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    malformed = b'{"type":"note","data":{"text":"caf\xc3"}}\n'
    expired = (_event("claim_released", "2026-07-01T00:00:00Z") + "\n").encode()
    events.write_bytes(malformed + expired)

    result = gc_events(events, now=NOW, ttl_hours=672)

    assert result == {"scanned": 2, "deleted": 1, "kept": 1, "malformed": 1}
    assert events.read_bytes() == malformed


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
