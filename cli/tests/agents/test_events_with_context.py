"""Tests for emit_with_context — Task 1.2 from observability spec.

Locks in:
- emit_with_context flattens all 13 EventContext fields into the record
- Legacy emit(kind, **data) byte-identical to baseline (regression guard)
- ts/kind are last-write-wins (cannot be overridden by ctx or data)
- Open **data kwargs land on top of ctx fields (Locked Decision #1)
"""
from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any

import pytest

from fno.paths_testing import use_tmpdir


EVENT_CONTEXT_FIELD_NAMES = {
    "from_name", "from_provider", "from_session_id", "from_cwd", "from_pid",
    "caller_kind",
    "to_name", "to_provider", "to_cwd", "to_session_id", "transport",
    "request_id", "target_session_id",
}


def _make_ctx(**overrides: Any):
    """Build a baseline EventContext, optionally overriding fields."""
    from fno.agents.context import EventContext

    base = dict(
        from_name="fno",
        from_provider=None,
        from_session_id=None,
        from_cwd="/cwd",
        from_pid=42,
        caller_kind="human_cli",
        to_name="recv",
        to_provider="codex",
        to_cwd=None,
        to_session_id=None,
        transport="direct-cli",
        request_id="abcd" * 8,  # 32 hex chars
        target_session_id=None,
    )
    base.update(overrides)
    return EventContext(**base)


def test_emit_with_context_writes_one_jsonl_line(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """emit_with_context appends exactly one JSON line, well-formed."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.events import emit_with_context

    events_path = tmp_path / ".fno" / "events.jsonl"
    ctx = _make_ctx()
    emit_with_context(ctx, "agent_ask_started", path=events_path)

    lines = events_path.read_text(encoding="utf-8").splitlines()
    assert len(lines) == 1
    parsed = json.loads(lines[0])
    assert parsed["kind"] == "agent_ask_started"
    assert "ts" in parsed


def test_emit_with_context_includes_all_13_context_fields(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """All 13 EventContext fields appear flattened on the JSONL record."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.events import emit_with_context

    events_path = tmp_path / ".fno" / "events.jsonl"
    ctx = _make_ctx(
        from_name="parent-agent",
        from_provider="claude",
        from_session_id="sess-abc",
        caller_kind="nested_agent",
        to_session_id="recv-sess",
        transport="mcp",
        target_session_id="target-xyz",
    )
    emit_with_context(ctx, "agent_ask_done", path=events_path)

    record = json.loads(events_path.read_text(encoding="utf-8").strip())
    for name in EVENT_CONTEXT_FIELD_NAMES:
        assert name in record, f"missing flattened field: {name}"
    assert record["from_name"] == "parent-agent"
    assert record["caller_kind"] == "nested_agent"
    assert record["target_session_id"] == "target-xyz"
    assert record["transport"] == "mcp"


def test_emit_with_context_kwargs_extend_record(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Open **data kwargs flatten alongside ctx fields."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.events import emit_with_context

    events_path = tmp_path / ".fno" / "events.jsonl"
    ctx = _make_ctx()
    emit_with_context(
        ctx, "agent_ask_done",
        path=events_path,
        duration_ms=1234, reply_chars=567, backend="direct-cli",
    )
    record = json.loads(events_path.read_text(encoding="utf-8").strip())
    assert record["duration_ms"] == 1234
    assert record["reply_chars"] == 567
    assert record["backend"] == "direct-cli"


def test_emit_with_context_kwargs_override_context_fields(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Locked Decision #1: **data lands on top of ctx fields (last-write-wins)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.events import emit_with_context

    events_path = tmp_path / ".fno" / "events.jsonl"
    ctx = _make_ctx(to_session_id="from-ctx")
    emit_with_context(
        ctx, "agent_ask_done", path=events_path, to_session_id="from-kwargs"
    )
    record = json.loads(events_path.read_text(encoding="utf-8").strip())
    assert record["to_session_id"] == "from-kwargs"


def test_emit_with_context_ts_cannot_be_overridden_via_data(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """``ts`` is last-write-wins; passing ``ts`` in **data cannot shadow it.

    (``kind`` is a named parameter — Python's TypeError on duplicate
    kwargs is the structural defense there; this only covers ``ts``.)
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.events import emit_with_context

    events_path = tmp_path / ".fno" / "events.jsonl"
    ctx = _make_ctx()
    emit_with_context(ctx, "agent_ask_done", path=events_path, ts="HIJACK")
    record = json.loads(events_path.read_text(encoding="utf-8").strip())
    assert record["kind"] == "agent_ask_done"
    assert record["ts"] != "HIJACK"
    assert re.match(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$", record["ts"])


def test_legacy_emit_byte_identical_baseline(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Regression guard: legacy emit(kind, **data) output unchanged.

    Pinned shape: keys in the JSON record are *exactly* the data kwargs
    followed by ts then kind. No extra fields, no reordering.
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.events import emit

    events_path = tmp_path / ".fno" / "events.jsonl"
    emit("agent_ask_done", path=events_path, name="foo", provider="codex", reply_chars=42)
    line = events_path.read_text(encoding="utf-8").strip()
    record = json.loads(line)
    # Shape: data kwargs first, then ts, then kind. No ctx flattening.
    assert list(record.keys()) == ["name", "provider", "reply_chars", "ts", "kind"]
    assert record["kind"] == "agent_ask_done"
    assert record["name"] == "foo"


def test_emit_with_context_swallows_oserror(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """Telemetry emission is best-effort; OSError is warned + swallowed."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.events import emit_with_context

    # Point at a path under a regular file (mkdir() will fail with NotADirectoryError).
    blocker = tmp_path / "blocker"
    blocker.write_text("not a dir")
    bad_path = blocker / "events.jsonl"

    ctx = _make_ctx()
    emit_with_context(ctx, "agent_ask_done", path=bad_path)  # must not raise

    err = capsys.readouterr().err
    assert "warning" in err.lower() or "warn" in err.lower()
