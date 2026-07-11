"""Tests for ``fno agents trace`` (trace_logic MVP).

Task 3.3 from 2026-05-22-fno-agents-observability.md.

Covers:
- AC1-HP: interleaved timeline, sorted by ts.
- AC1-ERR: missing agent → exit 13 with stderr.
- AC1-UI: --json output carries from_name/caller_kind/request_id.
- AC1-EDGE: zero events → "no events yet" + exit 0.
- AC4-EDGE: orphan _started shows "no _done received" marker.
- AC4-UI: 8-char request_id prefix in default; 32-char in --json.
- AC5-UI: target_session_id surfaced as timeline header line.

Deferred to follow-up (NOT covered here): --follow streaming
(AC1-FR), transport-demote markers (AC4-FR).
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest


def _write_events(path: Path, records: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(json.dumps(r, separators=(",", ":")) for r in records) + "\n")


_FULL_RID = "a1b2c3d4e5f6789012345678901234ab"


def _started(rid: str, to_name: str = "alpha", ts: str = "2026-05-22T10:00:00Z", **extra) -> dict:
    return {
        "ts": ts,
        "kind": "agent_ask_started",
        "to_name": to_name,
        "from_name": "fno",
        "caller_kind": "human_cli",
        "request_id": rid,
        **extra,
    }


def _done(rid: str, to_name: str = "alpha", ts: str = "2026-05-22T10:00:01Z", **extra) -> dict:
    return {
        "ts": ts,
        "kind": "agent_ask_done",
        "to_name": to_name,
        "from_name": "fno",
        "caller_kind": "human_cli",
        "request_id": rid,
        **extra,
    }


def _unified_daemon(kind: str, name: str = "alpha", ts: str = "2026-05-22T10:00:03Z", **data) -> dict:
    """A daemon EventEmitter line in the unified x-2901 envelope: type + data.

    daemon.rs emits agent lifecycle lines like agent_ask_done as
    {ts, type, source, data:{name, ...}} (payload nested, `type` not `kind`).
    """
    return {"ts": ts, "type": kind, "source": "daemon", "data": {"name": name, **data}}


# ---------------------------------------------------------------------------
# x-2901 — the unified {type, data} daemon envelope renders (fallback)
# ---------------------------------------------------------------------------


def test_trace_renders_unified_daemon_line(tmp_path: Path) -> None:
    """A daemon EventEmitter line (type + nested data) is matched by name and
    rendered, not silently filtered. Regression guard for the x-2901 emit cut:
    before the reader fallback, trace read only top-level kind/name and went
    blind to the new envelope."""
    from fno.agents.trace_cli import trace_logic

    events_path = tmp_path / "events.jsonl"
    _write_events(events_path, [_unified_daemon("agent_ask_done", name="alpha", backend="pty")])
    res = trace_logic(name="alpha", events_path=events_path, registry_check=False)
    assert res.exit_code == 0
    assert "agent_ask_done" in res.output, res.output
    assert "no events yet" not in res.output


def test_trace_mixed_envelopes_both_render(tmp_path: Path) -> None:
    """A flat audit line (append_agents_event) and a unified daemon line for the
    same agent both render during the mixed-binary window."""
    from fno.agents.trace_cli import trace_logic

    events_path = tmp_path / "events.jsonl"
    _write_events(events_path, [
        _started(rid=_FULL_RID, to_name="alpha", ts="2026-05-22T10:00:00Z"),
        _unified_daemon("agent_ask_done", name="alpha", ts="2026-05-22T10:00:01Z"),
    ])
    res = trace_logic(name="alpha", events_path=events_path, registry_check=False)
    assert res.exit_code == 0
    assert "agent_ask_started" in res.output
    assert "agent_ask_done" in res.output


# ---------------------------------------------------------------------------
# AC1-HP — interleaved timeline sorted by ts
# ---------------------------------------------------------------------------


def test_trace_returns_events_sorted_by_ts(tmp_path: Path) -> None:
    from fno.agents.trace_cli import trace_logic

    events_path = tmp_path / "events.jsonl"
    _write_events(events_path, [
        _done(rid=_FULL_RID, ts="2026-05-22T10:00:02Z"),
        _started(rid=_FULL_RID, ts="2026-05-22T10:00:00Z"),
    ])
    res = trace_logic(name="alpha", events_path=events_path, registry_check=False)
    assert res.exit_code == 0
    lines = [l for l in res.output.splitlines() if l]
    # First non-header line is the started (earlier ts) per sort.
    assert "agent_ask_started" in lines[0]
    assert "agent_ask_done" in lines[-1]


# ---------------------------------------------------------------------------
# AC1-ERR — missing agent → exit 13
# ---------------------------------------------------------------------------


def test_trace_missing_agent_exits_13(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fno.agents.trace_cli import trace_logic

    # Stub registry-check to report "not registered" — simulating a
    # name the user typed that doesn't match any live agent.
    from fno.agents import trace_cli as trace_mod
    monkeypatch.setattr(trace_mod, "_agent_exists_in_registry", lambda _n: False)

    res = trace_logic(name="ghost", events_path=tmp_path / "events.jsonl")
    assert res.exit_code == 13
    assert "ghost" in res.stderr
    assert "not found" in res.stderr


def test_trace_all_agents_skips_registry_gate(tmp_path: Path) -> None:
    """--all bypasses the registry gate (operator may want to inspect across agents)."""
    from fno.agents.trace_cli import trace_logic

    events_path = tmp_path / "events.jsonl"
    _write_events(events_path, [_started(rid=_FULL_RID, to_name="anyone")])
    res = trace_logic(
        name=None,
        all_agents=True,
        events_path=events_path,
        registry_check=False,
    )
    assert res.exit_code == 0


# ---------------------------------------------------------------------------
# AC1-UI — --json output carries the full envelope
# ---------------------------------------------------------------------------


def test_trace_json_carries_context_fields(tmp_path: Path) -> None:
    from fno.agents.trace_cli import trace_logic

    events_path = tmp_path / "events.jsonl"
    _write_events(events_path, [
        _started(
            rid=_FULL_RID,
            caller_kind="nested_agent",
            from_name="parent",
            from_session_id="sess-1",
        ),
    ])
    res = trace_logic(name="alpha", json_out=True, events_path=events_path, registry_check=False)
    assert res.exit_code == 0
    line = res.output.strip()
    record = json.loads(line)
    assert record["from_name"] == "parent"
    assert record["caller_kind"] == "nested_agent"
    assert record["request_id"] == _FULL_RID
    assert record["from_session_id"] == "sess-1"


# ---------------------------------------------------------------------------
# AC1-EDGE — zero events
# ---------------------------------------------------------------------------


def test_trace_zero_events_exits_0_with_message(tmp_path: Path) -> None:
    from fno.agents.trace_cli import trace_logic

    res = trace_logic(
        name="alpha",
        events_path=tmp_path / "absent.jsonl",
        registry_check=False,
    )
    assert res.exit_code == 0
    assert "no events yet" in res.output


# ---------------------------------------------------------------------------
# AC4-EDGE — orphan _started shows "no _done received"
# ---------------------------------------------------------------------------


def test_trace_orphan_started_shows_marker(tmp_path: Path) -> None:
    from fno.agents.trace_cli import trace_logic

    events_path = tmp_path / "events.jsonl"
    _write_events(events_path, [_started(rid=_FULL_RID)])
    res = trace_logic(name="alpha", events_path=events_path, registry_check=False)
    assert "no _done received" in res.output


def test_trace_matched_started_done_no_orphan_marker(tmp_path: Path) -> None:
    from fno.agents.trace_cli import trace_logic

    events_path = tmp_path / "events.jsonl"
    _write_events(events_path, [
        _started(rid=_FULL_RID, ts="2026-05-22T10:00:00Z"),
        _done(rid=_FULL_RID, ts="2026-05-22T10:00:01Z"),
    ])
    res = trace_logic(name="alpha", events_path=events_path, registry_check=False)
    assert "no _done received" not in res.output


# ---------------------------------------------------------------------------
# AC4-UI — 8-char request_id prefix in default; 32-char in --json
# ---------------------------------------------------------------------------


def test_trace_default_human_truncates_request_id_to_8(tmp_path: Path) -> None:
    from fno.agents.trace_cli import trace_logic

    events_path = tmp_path / "events.jsonl"
    _write_events(events_path, [
        _started(rid=_FULL_RID, ts="2026-05-22T10:00:00Z"),
        _done(rid=_FULL_RID, ts="2026-05-22T10:00:01Z"),
    ])
    res = trace_logic(name="alpha", events_path=events_path, registry_check=False)
    # The 32-char full rid must NOT appear in human output.
    assert _FULL_RID not in res.output
    # The 8-char prefix MUST appear.
    assert _FULL_RID[:8] in res.output


def test_trace_json_keeps_full_request_id(tmp_path: Path) -> None:
    from fno.agents.trace_cli import trace_logic

    events_path = tmp_path / "events.jsonl"
    _write_events(events_path, [_started(rid=_FULL_RID)])
    res = trace_logic(name="alpha", json_out=True, events_path=events_path, registry_check=False)
    assert _FULL_RID in res.output


# ---------------------------------------------------------------------------
# Filter by request_id (cross-agent join)
# ---------------------------------------------------------------------------


def test_trace_filter_by_request_id_joins_across_agents(tmp_path: Path) -> None:
    from fno.agents.trace_cli import trace_logic

    events_path = tmp_path / "events.jsonl"
    _write_events(events_path, [
        _started(rid=_FULL_RID, to_name="alpha"),
        _started(rid="other00000000000000000000000000ff", to_name="beta"),
        _done(rid=_FULL_RID, to_name="alpha"),
    ])
    res = trace_logic(
        name=None,
        all_agents=True,
        request_id=_FULL_RID,
        events_path=events_path,
        registry_check=False,
    )
    # 2 matching events (started + done for _FULL_RID); the beta started
    # must be filtered out.
    assert res.exit_code == 0
    assert "beta" not in res.output
    assert res.output.count("alpha") == 2


# ---------------------------------------------------------------------------
# AC5-UI — target_session_id surfaced as timeline header
# ---------------------------------------------------------------------------


def test_trace_surfaces_target_session_header(tmp_path: Path) -> None:
    from fno.agents.trace_cli import trace_logic

    events_path = tmp_path / "events.jsonl"
    _write_events(events_path, [
        _started(rid=_FULL_RID, target_session_id="target-xyz-1"),
        _done(rid=_FULL_RID, target_session_id="target-xyz-1"),
    ])
    res = trace_logic(name="alpha", events_path=events_path, registry_check=False)
    assert "target_session: target-xyz-1" in res.output


# ---------------------------------------------------------------------------
# Sigma-review fixes — regression guards
# ---------------------------------------------------------------------------


def test_trace_no_false_orphan_when_done_pushed_beyond_limit(tmp_path: Path) -> None:
    """sigma-review H1: started/done straddling --limit must not mis-orphan.

    Pre-fix: orphan detection ran AFTER slicing, so a done at index
    >=limit got dropped, and the surviving started was wrongly flagged
    "no _done received". This test pins the post-fix behavior: orphan
    detection uses the pre-slice filtered set.

    Setup: started + done for one rid, sorted such that done falls
    beyond ``limit=1``. The started survives the slice. Naive (broken)
    orphan detection would mark the started as orphan because the done
    is no longer visible to the seen_done scan.
    """
    from fno.agents.trace_cli import trace_logic

    events_path = tmp_path / "events.jsonl"
    _write_events(events_path, [
        _started(rid=_FULL_RID, ts="2026-05-22T10:00:00Z"),
        _done(rid=_FULL_RID, ts="2026-05-22T10:00:01Z"),
    ])
    res = trace_logic(
        name="alpha",
        events_path=events_path,
        registry_check=False,
        limit=1,  # drops the done
    )
    # Only the started row remains, but it should NOT be flagged as
    # orphan because the done was visible in the pre-slice set.
    assert "no _done received" not in res.output, (
        f"orphan detection was biased by the --limit window: {res.output!r}"
    )


def test_trace_requires_name_unless_all_set(tmp_path: Path) -> None:
    """Codex P2 round 2: bare `fno agents trace` (no name, no --all) must exit 2.

    Pre-fix silently dropped the name filter and returned every agent's
    events — contradicting the command help text.
    """
    from fno.agents.trace_cli import trace_logic

    res = trace_logic(
        name=None,
        all_agents=False,  # default — must be explicit
        events_path=tmp_path / "events.jsonl",
        registry_check=False,
    )
    assert res.exit_code == 2
    assert "required" in res.stderr.lower()


def test_trace_surfaces_registry_read_failure(tmp_path: Path) -> None:
    """Codex P2 round 2: a permission/corruption error in load_registry must

    surface as exit 12 with a "registry" stderr message, NOT a misleading
    "agent not found" (exit 13).
    """
    import pytest as _pytest
    from fno.agents.trace_cli import trace_logic, _RegistryReadError
    from fno.agents import trace_cli as trace_mod

    def boom(_name: str) -> bool:
        raise _RegistryReadError("permission denied reading registry")

    _pytest.MonkeyPatch().setattr(trace_mod, "_agent_exists_in_registry", boom)
    res = trace_logic(
        name="alpha",
        events_path=tmp_path / "events.jsonl",
    )
    assert res.exit_code == 12
    assert "registry" in res.stderr.lower()
    assert "not found" not in res.stderr


def test_trace_since_parses_datetime_variants(tmp_path: Path) -> None:
    """Codex P2 round 2: --since must handle ISO8601 variants, not raw-string compare.

    The pre-fix raw-string compare worked only for the canonical
    YYYY-MM-DDTHH:MM:SSZ shape; a user passing `2026-05-22T10:00:00+00:00`
    (same instant, equivalent format) would be wrongly filtered.
    """
    from fno.agents.trace_cli import trace_logic

    events_path = tmp_path / "events.jsonl"
    _write_events(events_path, [
        _started(rid=_FULL_RID, ts="2026-05-22T10:00:00Z"),
        _done(rid=_FULL_RID, ts="2026-05-22T10:00:30Z"),
    ])
    # since with +00:00 offset should include both events (both >= cutoff).
    res = trace_logic(
        name="alpha",
        events_path=events_path,
        registry_check=False,
        since="2026-05-22T09:59:00+00:00",
    )
    assert res.exit_code == 0
    # Both events should appear.
    assert "agent_ask_started" in res.output
    assert "agent_ask_done" in res.output


def test_trace_warns_on_malformed_jsonl_lines(tmp_path: Path) -> None:
    """sigma-review H2: malformed events.jsonl rows must surface a stderr warn."""
    from fno.agents.trace_cli import trace_logic

    events_path = tmp_path / "events.jsonl"
    events_path.parent.mkdir(parents=True, exist_ok=True)
    valid = json.dumps(_started(rid=_FULL_RID))
    events_path.write_text(
        valid + "\n"
        + "{this is not json\n"
        + "[also not a dict]\n"
        + "\n"  # blank line, not malformed
    )
    res = trace_logic(name="alpha", events_path=events_path, registry_check=False)
    # Should still emit the 1 valid row.
    assert res.exit_code == 0
    assert _FULL_RID[:8] in res.output
    # And surface the 2 malformed rows to stderr.
    assert "skipped 2 malformed" in res.stderr
