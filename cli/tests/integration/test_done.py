"""Integration tests for `fno done` race collision and force-overwrite.

Covers:
- #28: done_race_collision event emitted when second fno done call sees _status already done.
       User-supplied --link/--note/--pr still applied. _status/completed_at NOT overwritten.
- #30: --force-overwrite flag causes _apply_rollup to overwrite even non-null rollup fields.
       Default (fill-if-null) behavior is the control case.
"""
from __future__ import annotations

import json
import re
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


# -- shared fixtures/helpers --


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    """Fresh graph.json + ledger.json routed via monkeypatch."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    return g


@pytest.fixture
def tmp_ledger(tmp_path) -> Path:
    return tmp_path / "ledger.json"


@pytest.fixture
def tmp_events(tmp_path, monkeypatch) -> Path:
    """Route append_event to a temp events.jsonl."""
    events_file = tmp_path / "events.jsonl"
    import fno.events as ev_mod
    monkeypatch.setattr(ev_mod, "append_event", _make_append_event_to(events_file))
    return events_file


def _make_append_event_to(events_file: Path):
    """Return a patched append_event that writes to a specific events_file."""
    import fno.events as ev_mod

    def _append(event, path=None, *, lock_timeout_seconds=30):
        # Always write to our temp file regardless of the path argument.
        ev_mod.validate(event)
        events_file.parent.mkdir(parents=True, exist_ok=True)
        import json
        with events_file.open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(event, separators=(",", ":")) + "\n")

    return _append


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _read(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


def _seed_ledger(ledger: Path, entries: list[dict]) -> None:
    ledger.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _stub_subprocess_no_git(monkeypatch):
    """Stub out git and gh so they return nothing (no branch/PR detection)."""
    from fno.done import cli as done_cli

    class _Res:
        def __init__(self, stdout="", rc=1):
            self.stdout = stdout
            self.returncode = rc
            self.stderr = ""

    monkeypatch.setattr(done_cli.subprocess, "run", lambda *a, **kw: _Res())


# ---- #28: done_race_collision ----


def test_done_race_collision_emits_event_and_preserves_metadata(
    tmp_graph, tmp_ledger, tmp_events, monkeypatch
):
    """#28: second fno done on an already-done node:
    - emits done_race_collision event
    - applies user-supplied --link
    - does NOT overwrite _status or completed_at
    """
    first_completed_at = "2026-05-15T10:00:00+00:00"
    _seed(tmp_graph, [{
        "id": "ab-race001",
        "title": "Race test node",
        "_status": "done",
        "completed_at": first_completed_at,
        "domain": "research",
        "artifact_url": None,
    }])
    _stub_subprocess_no_git(monkeypatch)

    result = runner.invoke(app, ["done", "ab-race001", "--link", "https://example.com/artifact"])
    # Should succeed (exit 0) with a diagnostic to stderr
    assert result.exit_code == 0, f"Exit {result.exit_code}: {result.output}"

    # _status and completed_at must NOT change
    entry = next(e for e in _read(tmp_graph) if e["id"] == "ab-race001")
    assert entry["_status"] == "done"
    assert entry["completed_at"] == first_completed_at

    # --link should still be applied
    assert entry.get("artifact_url") == "https://example.com/artifact"

    # done_race_collision event must be in events.jsonl
    assert tmp_events.exists(), "events.jsonl was not created"
    events = [json.loads(line) for line in tmp_events.read_text().splitlines() if line.strip()]
    collision_events = [e for e in events if e.get("type") == "done_race_collision"]
    assert collision_events, f"No done_race_collision event found. Events: {events}"

    ev = collision_events[0]
    data = ev.get("data", {})
    assert data.get("node_id") == "ab-race001"
    assert data.get("first_completed_at") == first_completed_at
    assert data.get("second_attempt_at")  # non-empty timestamp


def test_done_race_collision_stderr_diagnostic(
    tmp_graph, tmp_ledger, tmp_events, monkeypatch
):
    """#28: stderr diagnostic must contain full canonical phrase on collision.

    Format: ``fno done: <id> already done at <ts>; metadata updates applied;
    collision event emitted`` (or ``... emit failed: <exc>`` on emit failure).
    Reflects actual emit outcome - the diagnostic prints AFTER the emit
    completes (per memory feedback_forward_promise_telemetry_lies).
    """
    first_completed_at = "2026-05-15T10:00:00+00:00"
    _seed(tmp_graph, [{
        "id": "ab-race002",
        "title": "Another race node",
        "_status": "done",
        "completed_at": first_completed_at,
        "domain": "research",
    }])
    _stub_subprocess_no_git(monkeypatch)

    result = runner.invoke(app, ["done", "ab-race002", "--link", "https://example.com/x"])
    assert result.exit_code == 0

    combined = result.output + (result.stderr or "")
    expected = (
        f"fno done: ab-race002 already done at {first_completed_at}; "
        "metadata updates applied; collision event emitted"
    )
    assert expected in combined, (
        f"Expected exact diagnostic {expected!r} not found in output: {combined!r}"
    )


def test_done_race_collision_applies_pr_and_note(
    tmp_graph, tmp_ledger, tmp_events, monkeypatch
):
    """#28: second fno done on done node applies --pr and --note while preserving _status/completed_at."""
    first_completed_at = "2026-05-15T10:00:00+00:00"
    _seed(tmp_graph, [{
        "id": "ab-race003",
        "title": "PR/note race node",
        "_status": "done",
        "completed_at": first_completed_at,
        "domain": "code",
    }])
    _stub_subprocess_no_git(monkeypatch)

    result = runner.invoke(
        app,
        ["done", "ab-race003", "--pr", "42", "--note", "second pass"],
    )
    assert result.exit_code == 0

    entry = next(e for e in _read(tmp_graph) if e["id"] == "ab-race003")
    assert entry.get("pr_number") == 42
    assert entry.get("merge_status") == "merged"
    assert entry.get("completion_note") == "second pass"
    # _status / completed_at preserved despite the metadata writes.
    assert entry.get("_status") == "done"
    assert entry.get("completed_at") == first_completed_at


def test_done_no_collision_when_not_done(
    tmp_graph, tmp_ledger, tmp_events, monkeypatch
):
    """#28 (control): no done_race_collision event when node is not yet done."""
    _seed(tmp_graph, [{
        "id": "ab-norace001",
        "title": "Normal node",
        "_status": "ready",
        "domain": "research",
    }])
    _stub_subprocess_no_git(monkeypatch)

    result = runner.invoke(app, ["done", "ab-norace001", "--link", "https://example.com/ok"])
    assert result.exit_code == 0

    # No collision event
    if tmp_events.exists():
        events = [json.loads(line) for line in tmp_events.read_text().splitlines() if line.strip()]
        collision_events = [e for e in events if e.get("type") == "done_race_collision"]
        assert not collision_events, f"Unexpected done_race_collision: {collision_events}"

    # Node should now be done
    entry = next(e for e in _read(tmp_graph) if e["id"] == "ab-norace001")
    assert entry["_status"] == "done"


# ---- #30: --force-overwrite ----


def test_done_force_overwrite(tmp_graph, tmp_ledger, monkeypatch):
    """#30: --force-overwrite causes session_id to be overwritten even if already set."""
    existing_session = "existing-session-aabbccdd"
    new_session = "new-session-11223344"
    plan_path = "/some/plan.md"

    _seed(tmp_graph, [{
        "id": "ab-fo001",
        "title": "Force overwrite node",
        "_status": "done",
        "completed_at": "2026-05-15T10:00:00+00:00",
        "domain": "research",
        "plan_path": plan_path,
        "session_id": existing_session,
    }])

    _seed_ledger(tmp_ledger, [{
        "plan_path": plan_path,
        "sessions": [new_session],
        "cost_usd": 0.05,
        "completed": "2026-05-15T12:00:00Z",
    }])
    _stub_subprocess_no_git(monkeypatch)

    result = runner.invoke(app, ["done", "ab-fo001", "--backfill", "--force-overwrite"])
    assert result.exit_code == 0, f"Exit {result.exit_code}: {result.output}"

    entry = next(e for e in _read(tmp_graph) if e["id"] == "ab-fo001")
    # With --force-overwrite, the session_id should be overwritten from ledger
    assert entry.get("session_id") == new_session, (
        f"Expected session_id={new_session!r}, got {entry.get('session_id')!r}"
    )


def test_done_force_overwrite_default_is_fill_if_null(tmp_graph, tmp_ledger, monkeypatch):
    """#30 (control): default (no --force-overwrite) preserves existing session_id."""
    existing_session = "existing-session-aabbccdd"
    new_session = "new-session-11223344"
    plan_path = "/some/plan.md"

    _seed(tmp_graph, [{
        "id": "ab-fo002",
        "title": "Fill if null control node",
        "_status": "done",
        "completed_at": "2026-05-15T10:00:00+00:00",
        "domain": "research",
        "plan_path": plan_path,
        "session_id": existing_session,
    }])

    _seed_ledger(tmp_ledger, [{
        "plan_path": plan_path,
        "sessions": [new_session],
        "cost_usd": 0.05,
        "completed": "2026-05-15T12:00:00Z",
    }])
    _stub_subprocess_no_git(monkeypatch)

    result = runner.invoke(app, ["done", "ab-fo002", "--backfill"])
    assert result.exit_code == 0, f"Exit {result.exit_code}: {result.output}"

    entry = next(e for e in _read(tmp_graph) if e["id"] == "ab-fo002")
    # Without --force-overwrite, the existing session_id must be preserved
    assert entry.get("session_id") == existing_session, (
        f"Expected existing session_id to be preserved, got {entry.get('session_id')!r}"
    )


_ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")


def test_help_lists_force_overwrite(monkeypatch):
    """AC-UI: `fno done --help` shows --force-overwrite option.

    Strips ANSI escape sequences before the substring check. Rich's help
    renderer interleaves style codes between characters of option names
    in some terminal configurations (notably GitHub Actions Ubuntu
    runners), so the literal `--force-overwrite` substring is absent
    from the raw output even when the rendered help clearly shows it.
    """
    result = runner.invoke(app, ["done", "--help"])
    assert result.exit_code == 0
    plain = _ANSI_ESCAPE_RE.sub("", result.output)
    assert "--force-overwrite" in plain, (
        f"--force-overwrite not found in help output: {result.output}"
    )


def test_done_collision_with_force_overwrite_applies_rollup(
    tmp_graph, tmp_ledger, tmp_events, monkeypatch
):
    """#28 + #30 interaction: collision path with --force-overwrite still applies rollup.

    Gemini HIGH on PR #279: the collision branch must honor --force-overwrite
    by re-applying the ledger rollup, otherwise the flag's promise (explicit
    re-reconciliation of stale rollups) is silently broken on second-writer
    calls. A bare `fno done <id>` on a done node should NOT touch rollup;
    `fno done <id> --force-overwrite` SHOULD overwrite session_id from ledger.
    """
    plan_path = "/some/plan.md"
    existing_session = "stale-session-aaaaaaaa"
    new_session = "fresh-session-bbbbbbbb"
    first_completed_at = "2026-05-15T10:00:00+00:00"

    _seed(tmp_graph, [{
        "id": "ab-force-collide-001",
        "title": "Collision + force_overwrite",
        "_status": "done",
        "completed_at": first_completed_at,
        "domain": "research",
        "plan_path": plan_path,
        "session_id": existing_session,
    }])
    _seed_ledger(tmp_ledger, [{
        "plan_path": plan_path,
        "sessions": [new_session],
        "cost_usd": 0.05,
        "completed": "2026-05-15T12:00:00Z",
    }])
    _stub_subprocess_no_git(monkeypatch)

    # --link satisfies the domain gate so the call reaches the mutator.
    result = runner.invoke(
        app,
        ["done", "ab-force-collide-001", "--link", "https://example.com/y", "--force-overwrite"],
    )
    assert result.exit_code == 0, f"Exit {result.exit_code}: {result.output}"

    entry = next(e for e in _read(tmp_graph) if e["id"] == "ab-force-collide-001")
    # --force-overwrite on collision: session_id overwritten from ledger.
    assert entry.get("session_id") == new_session, (
        f"Expected --force-overwrite to overwrite session_id even on collision, "
        f"got {entry.get('session_id')!r}"
    )
    # --link still applied (metadata writes survive force-overwrite on collision).
    assert entry.get("artifact_url") == "https://example.com/y"
    # _status / completed_at still preserved (collision invariant).
    assert entry.get("_status") == "done"
    assert entry.get("completed_at") == first_completed_at
    # Collision event still emitted.
    if tmp_events.exists():
        events = [json.loads(line) for line in tmp_events.read_text().splitlines() if line.strip()]
        collision_events = [e for e in events if e.get("type") == "done_race_collision"]
        assert collision_events, "Expected done_race_collision event on collision + force_overwrite"


def test_done_collision_without_force_overwrite_skips_rollup(
    tmp_graph, tmp_ledger, monkeypatch
):
    """#28 control: bare collision (no --force-overwrite) must NOT touch rollup."""
    plan_path = "/some/plan.md"
    existing_session = "stale-session-aaaaaaaa"
    new_session = "fresh-session-bbbbbbbb"

    _seed(tmp_graph, [{
        "id": "ab-bare-collide-001",
        "title": "Bare collision",
        "_status": "done",
        "completed_at": "2026-05-15T10:00:00+00:00",
        "domain": "research",
        "plan_path": plan_path,
        "session_id": existing_session,
    }])
    _seed_ledger(tmp_ledger, [{
        "plan_path": plan_path,
        "sessions": [new_session],
        "cost_usd": 0.05,
        "completed": "2026-05-15T12:00:00Z",
    }])
    _stub_subprocess_no_git(monkeypatch)

    result = runner.invoke(app, ["done", "ab-bare-collide-001", "--link", "https://example.com/z"])
    assert result.exit_code == 0, f"Exit {result.exit_code}: {result.output}"

    entry = next(e for e in _read(tmp_graph) if e["id"] == "ab-bare-collide-001")
    # Bare collision: session_id preserved (rollup NOT touched).
    assert entry.get("session_id") == existing_session, (
        f"Expected bare collision to preserve existing session_id, "
        f"got {entry.get('session_id')!r}"
    )


def test_done_force_overwrite_backfill_preserves_historical_session(
    tmp_graph, tmp_ledger, monkeypatch
):
    """#30 + Codex P1 regression: --backfill --force-overwrite from an active
    session must trust the ledger, NOT CLAUDECODE_SESSION_ID.

    Without this guard, a backfill sweep from a live session would mass-rewrite
    every reconciled node's session_id to the CURRENT session id - the exact
    opposite of "reconcile historical attribution from the ledger." Codex P1
    on PR #279.
    """
    plan_path = "/some/plan.md"
    ledger_session = "ledger-historical-aabbccdd"
    current_env_session = "current-env-99887766"

    _seed(tmp_graph, [{
        "id": "ab-codex-p1-001",
        "title": "Codex P1 regression",
        "_status": "done",
        "completed_at": "2026-05-15T10:00:00+00:00",
        "domain": "research",
        "plan_path": plan_path,
        "session_id": "stale-existing-11111111",
    }])
    _seed_ledger(tmp_ledger, [{
        "plan_path": plan_path,
        "sessions": [ledger_session],
        "cost_usd": 0.05,
        "completed": "2026-05-15T12:00:00Z",
    }])
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", current_env_session)
    _stub_subprocess_no_git(monkeypatch)

    result = runner.invoke(app, ["done", "ab-codex-p1-001", "--backfill", "--force-overwrite"])
    assert result.exit_code == 0, f"Exit {result.exit_code}: {result.output}"

    entry = next(e for e in _read(tmp_graph) if e["id"] == "ab-codex-p1-001")
    # With --force-overwrite, ledger session must win over env session.
    assert entry.get("session_id") == ledger_session, (
        f"Expected ledger session {ledger_session!r} to win over env "
        f"{current_env_session!r}; got {entry.get('session_id')!r}. "
        "If this fails, --backfill --force-overwrite mass-rewrites history."
    )


def test_done_normal_mode_still_prefers_env_session(tmp_graph, tmp_ledger, monkeypatch):
    """#30 control: without --force-overwrite, env_session still takes
    precedence over ledger (normal first-time marking semantics unchanged).
    """
    plan_path = "/some/plan.md"
    ledger_session = "ledger-session-aabbccdd"
    current_env_session = "current-env-99887766"

    _seed(tmp_graph, [{
        "id": "ab-env-control-001",
        "title": "Env priority control",
        "_status": "ready",
        "domain": "research",
        "plan_path": plan_path,
    }])
    _seed_ledger(tmp_ledger, [{
        "plan_path": plan_path,
        "sessions": [ledger_session],
        "cost_usd": 0.05,
        "completed": "2026-05-15T12:00:00Z",
    }])
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", current_env_session)
    _stub_subprocess_no_git(monkeypatch)

    result = runner.invoke(app, ["done", "ab-env-control-001", "--link", "https://example.com/x"])
    assert result.exit_code == 0, f"Exit {result.exit_code}: {result.output}"

    entry = next(e for e in _read(tmp_graph) if e["id"] == "ab-env-control-001")
    # Normal mode (no --force-overwrite): env_session wins over ledger
    # (matches pre-existing first-time-marking semantics).
    assert entry.get("session_id") == current_env_session, (
        f"Expected env session {current_env_session!r} in normal mode; "
        f"got {entry.get('session_id')!r}"
    )


def test_done_force_overwrite_cost_dedup(tmp_graph, tmp_ledger, monkeypatch):
    """#30 AC-EDGE: --force-overwrite --backfill must NOT double-count cost.

    Dedup key is (session_id, timestamp). When the ledger row matches an existing
    cost_sessions entry on both fields, the merge loop must skip it regardless
    of force_overwrite. The total cost_usd must stay at the existing value.
    """
    session = "shared-session-77889900"
    plan_path = "/some/plan.md"
    timestamp = "2026-05-15T12:00:00Z"

    _seed(tmp_graph, [{
        "id": "ab-dedup001",
        "title": "Cost-dedup node",
        "_status": "done",
        "completed_at": "2026-05-15T10:00:00+00:00",
        "domain": "research",
        "plan_path": plan_path,
        "session_id": session,
        "cost_usd": 0.10,
        "cost_sessions": [{
            "session_id": session,
            "timestamp": timestamp,
            "cost_usd": 0.10,
        }],
    }])

    # Ledger row that, after rollup, produces the same (session_id, timestamp)
    # pair already present in cost_sessions. The dedup must skip the duplicate.
    _seed_ledger(tmp_ledger, [{
        "plan_path": plan_path,
        "sessions": [session],
        "cost_usd": 0.10,
        "completed": timestamp,
    }])
    _stub_subprocess_no_git(monkeypatch)

    result = runner.invoke(app, ["done", "ab-dedup001", "--backfill", "--force-overwrite"])
    assert result.exit_code == 0, f"Exit {result.exit_code}: {result.output}"

    entry = next(e for e in _read(tmp_graph) if e["id"] == "ab-dedup001")
    assert len(entry["cost_sessions"]) == 1, (
        f"Expected dedup to keep one row, got {entry['cost_sessions']!r}"
    )
    assert entry["cost_usd"] == 0.10, (
        f"Expected cost_usd preserved at 0.10, got {entry['cost_usd']!r}"
    )


def test_done_force_overwrite_empty_ledger_noop(tmp_graph, tmp_ledger, monkeypatch):
    """#30 AC-ERR: --force-overwrite with empty rollup is a no-op.

    Even with --force-overwrite, missing ledger data must not null out the
    existing session_id or points (the `if new_sid` and `if rollup.get('points')
    is not None` guards keep null overwrites from happening).
    """
    existing_session = "existing-session-99887766"
    plan_path = "/some/missing/plan.md"

    _seed(tmp_graph, [{
        "id": "ab-empty001",
        "title": "Empty ledger node",
        "_status": "done",
        "completed_at": "2026-05-15T10:00:00+00:00",
        "domain": "research",
        "plan_path": plan_path,
        "session_id": existing_session,
        "points": 5,
    }])

    # Empty ledger - no matching entries for plan_path
    _seed_ledger(tmp_ledger, [])
    _stub_subprocess_no_git(monkeypatch)

    result = runner.invoke(app, ["done", "ab-empty001", "--backfill", "--force-overwrite"])
    assert result.exit_code == 0, f"Exit {result.exit_code}: {result.output}"

    entry = next(e for e in _read(tmp_graph) if e["id"] == "ab-empty001")
    assert entry.get("session_id") == existing_session, (
        f"Expected existing session_id preserved on empty rollup, got {entry.get('session_id')!r}"
    )
    assert entry.get("points") == 5, (
        f"Expected existing points preserved on empty rollup, got {entry.get('points')!r}"
    )
