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


def _store_rows(events: Path) -> list[dict]:
    from fno.events.store_client import read_committed_lines

    return [json.loads(ln) for ln in read_committed_lines(events) if ln.strip()]


def _seed(events: Path, rows: list[tuple[str, str]]) -> None:
    """Commit rows straight into the store beside `events`."""
    from fno.events.store_client import emit_envelope

    for event_type, ts in rows:
        envelope = json.loads(_event(event_type, ts))
        emit_envelope(envelope, events)


def test_gc_deletes_only_expired_explicit_ephemeral_rows(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    _seed(
        events,
        [
            ("claim_acquired", "2026-01-01T00:00:00Z"),  # ephemeral, expired
            ("claim_released", "2026-01-02T00:00:00Z"),  # ephemeral, expired
            ("human_touch", "2026-01-01T00:00:00Z"),  # ephemeral, expired
            ("operator_decision", "2026-01-01T00:00:00Z"),  # durable, expired age
            ("human_touch", NOW.strftime("%Y-%m-%dT%H:%M:%SZ")),  # ephemeral, fresh
        ],
    )
    result = gc_events(events, now=NOW, ttl_hours=672)
    assert result["deleted"] == 3, "only the expired ephemeral rows leave"
    types = sorted(r["type"] for r in _store_rows(events))
    assert types == ["human_touch", "operator_decision"], (
        "the fresh ephemeral and the durable row survive"
    )


def test_gc_refuses_a_horizon_shorter_than_schema_minimum(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    _seed(events, [("claim_acquired", "2026-01-01T00:00:00Z")])
    with pytest.raises(ValueError, match="shorter than the schema minimum"):
        gc_events(events, now=NOW, ttl_hours=671)


def test_gc_refuses_the_unlocked_global_daemon_journal(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fno import paths

    global_journal = tmp_path / "global.jsonl"
    monkeypatch.setattr(paths, "global_events_json", lambda: global_journal)
    global_journal.touch()
    with pytest.raises(ValueError, match="global daemon journal"):
        gc_events(global_journal, now=NOW, ttl_hours=672)


def test_gc_dry_run_reports_without_rewriting(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    _seed(events, [("claim_acquired", "2026-01-01T00:00:00Z")])
    result = gc_events(events, now=NOW, ttl_hours=672, dry_run=True)
    assert result["deleted"] == 0
    assert len(_store_rows(events)) == 1, "the dry run deleted nothing"


def test_gc_on_a_journal_with_no_store_is_an_empty_history(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    result = gc_events(events, now=NOW, ttl_hours=672)
    assert result == {"scanned": 0, "deleted": 0, "kept": 0, "malformed": 0}


def test_gc_cli_reports_the_sql_fold(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    # The CLI ticks on the real clock, so the fresh row is dated from now.
    fresh = (NOW if os.environ.get("FNO_TEST_FIXED_NOW") else datetime.now(timezone.utc)).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )
    _seed(
        events,
        [
            ("claim_acquired", "2026-01-01T00:00:00Z"),
            ("human_touch", fresh),
        ],
    )
    runner = CliRunner()
    result = runner.invoke(
        event_cli,
        ["gc", "--events", str(events), "--ttl-hours", "672"],
    )
    assert result.exit_code == 0, result.output
    assert "deleted=1" in result.output
    assert "kept=1" in result.output
