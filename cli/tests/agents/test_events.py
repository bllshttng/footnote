"""Tests for fno.agents.events — TDD Red phase.

AC4 from Task 1.2: events.emit() writes well-formed JSON line to
~/.fno/events.jsonl per schema. Schema (per design doc):

    {"ts":"...","kind":"agent_ask_started","name":"...","provider":"...",...}

Tests lock down:
- emit() writes exactly one JSON line per call (append semantics).
- Each line is independently parseable JSON.
- Every event has ts (ISO8601), kind (string), and arbitrary data fields.
- Default path is paths.state_dir() / "events.jsonl"; the path arg overrides.
- Empty events.jsonl is auto-created (parent dir + empty file).
"""
from __future__ import annotations

import json
from datetime import datetime
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


def test_emit_writes_one_jsonl_line(tmp_path: Path, monkeypatch) -> None:
    """emit() appends exactly one JSON line per call."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.events import emit

    events_path = tmp_path / ".fno" / "events.jsonl"
    emit("agent_ask_started", name="foo", provider="claude", path=events_path)
    emit("agent_ask_done", name="foo", duration_ms=42, path=events_path)

    lines = events_path.read_text(encoding="utf-8").splitlines()
    assert len(lines) == 2
    for line in lines:
        parsed = json.loads(line)
        assert "ts" in parsed
        assert "kind" in parsed


def test_emit_includes_kind_and_data_fields(tmp_path: Path, monkeypatch) -> None:
    """emit() flattens kwargs into the top-level JSON object alongside ts/kind."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.events import emit

    events_path = tmp_path / ".fno" / "events.jsonl"
    emit(
        "agent_ask_done",
        name="bar",
        provider="codex",
        short_id="7c5dcf5d",
        duration_ms=1234,
        reply_chars=456,
        path=events_path,
    )
    line = events_path.read_text(encoding="utf-8").strip()
    parsed = json.loads(line)
    assert parsed["kind"] == "agent_ask_done"
    assert parsed["name"] == "bar"
    assert parsed["provider"] == "codex"
    assert parsed["short_id"] == "7c5dcf5d"
    assert parsed["duration_ms"] == 1234


def test_emit_ts_is_iso8601_utc(tmp_path: Path, monkeypatch) -> None:
    """emit() stamps ts in ISO8601 UTC form (ends in Z or +00:00)."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.events import emit

    events_path = tmp_path / ".fno" / "events.jsonl"
    emit("agent_ping", path=events_path)
    parsed = json.loads(events_path.read_text(encoding="utf-8").strip())
    ts = parsed["ts"]
    # Must parse as ISO8601
    assert ts.endswith("Z") or "+" in ts
    # And re-parsing yields a datetime (proves real ISO format, not garbage)
    normalized = ts.replace("Z", "+00:00")
    parsed_dt = datetime.fromisoformat(normalized)
    assert parsed_dt is not None


def test_emit_default_path_under_state_dir(tmp_path: Path, monkeypatch) -> None:
    """emit() with no path arg writes to paths.state_dir() / 'events.jsonl'."""
    use_tmpdir(monkeypatch, tmp_path)
    import fno.paths as paths
    from fno.agents.events import emit

    emit("agent_test", name="x")
    expected = paths.state_dir() / "events.jsonl"
    assert expected.exists()
    parsed = json.loads(expected.read_text(encoding="utf-8").strip())
    assert parsed["kind"] == "agent_test"


def test_emit_creates_parent_dir_if_missing(tmp_path: Path, monkeypatch) -> None:
    """emit() creates the parent directory when it doesn't exist yet."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.events import emit

    deep_path = tmp_path / "nested" / "deeper" / "events.jsonl"
    assert not deep_path.parent.exists()
    emit("agent_test", path=deep_path)
    assert deep_path.exists()


def test_emit_kind_is_required_positional(tmp_path: Path, monkeypatch) -> None:
    """kind is a required positional arg; calling without it is a TypeError."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.events import emit

    with pytest.raises(TypeError):
        emit()  # type: ignore[call-arg]


def test_emit_data_cannot_overwrite_ts(tmp_path: Path, monkeypatch) -> None:
    """A caller-supplied ``ts`` field via **data cannot overwrite the canonical timestamp.

    Per Gemini review on PR #288: the mandatory fields must win the merge.
    (``kind`` is the function's positional arg so Python rejects collisions
    at the call site already; ``ts`` is only captured via **data and needs
    explicit precedence in the dict construction.)
    """
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.events import emit

    events_path = tmp_path / ".fno" / "events.jsonl"
    emit(
        "agent_test",
        path=events_path,
        ts="HACKED",  # type: ignore[arg-type]
        useful_field="ok",
    )
    parsed = json.loads(events_path.read_text(encoding="utf-8").strip())
    assert parsed["kind"] == "agent_test"
    # ts must be a real ISO timestamp, not the user's override
    assert parsed["ts"] != "HACKED"
    assert parsed["ts"].endswith("Z") or "+" in parsed["ts"]
    # User's other fields still land
    assert parsed["useful_field"] == "ok"


def test_emit_swallows_oserror_and_warns(
    tmp_path: Path, monkeypatch, capsys
) -> None:
    """emit() is best-effort — an OSError from the filesystem is logged, not raised."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno.agents.events import emit

    # Point at an unwritable target (read-only parent).
    parent = tmp_path / "readonly"
    parent.mkdir()
    parent.chmod(0o500)
    target = parent / "events.jsonl"

    try:
        # Must not raise — telemetry failures cannot break primary ops.
        emit("agent_test", path=target)
    finally:
        parent.chmod(0o700)  # let pytest clean up

    captured = capsys.readouterr()
    assert "warning" in captured.err.lower()
    assert "agent_test" in captured.err
