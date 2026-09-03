"""Every ledger writer records ``sessions`` explicitly, and the marker never leaks.

Three writers could emit a row with no session identifier, and two of them did
it by OMITTING the ``sessions`` key - an omission indistinguishable from a run
that had no session. The marker replaces absence, the reconcile harvest fills
old rows from the graph node, and ``resolve_pr_sessions`` never returns the
marker as if it were a session id.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.cost import _register
from fno.cost._register import LEDGER_SESSION_UNRESOLVED, sessions_or_unresolved
from fno.ledger_join import resolve_pr_sessions

UUID = "0abcd123-4567-89ab-cdef-0123456789ab"


@pytest.fixture
def ledger_path(tmp_path, monkeypatch):
    """Point the register module's ledger at a temp file."""
    p = tmp_path / "ledger.json"
    monkeypatch.setattr(
        _register,
        "_paths",
        type(
            "_P",
            (),
            {
                "ledger_json": staticmethod(lambda: p),
                "resolve_canonical_worktree": staticmethod(lambda p, timeout=2: p),
            },
        ),
    )
    return p


def _write_ledger(path: Path, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"entries": rows}, indent=2))


# --- the shared helper -----------------------------------------------------


def test_helper_keeps_order_dedups_and_marks():
    assert sessions_or_unresolved("a", "b", "a", "", None) == ["a", "b"]
    assert sessions_or_unresolved(None, "", "  ") == [LEDGER_SESSION_UNRESOLVED]


# --- change 1: the three writers -------------------------------------------


def test_build_entry_records_the_manifest_uuid(ledger_path, monkeypatch):
    # Positive marker: the uuid itself must be PRESENT in the list. A green
    # assertion on "the row exists" proves nothing; rows always existed.
    monkeypatch.setattr(_register, "_registry_axes", lambda state: {})
    monkeypatch.setattr(_register, "_pr_number_from_gh", lambda cwd: None)
    entry = _register.build_entry({"harness_session_id": UUID, "fno_id": "tgt-1"}, "")
    assert UUID in entry["sessions"]


def test_build_entry_writes_marker_when_nothing_resolves(ledger_path, monkeypatch):
    monkeypatch.setattr(_register, "_registry_axes", lambda state: {})
    monkeypatch.setattr(_register, "_pr_number_from_gh", lambda cwd: None)
    entry = _register.build_entry({}, "")
    assert entry["sessions"] == [LEDGER_SESSION_UNRESOLVED]


def test_quick_entry_consults_the_ambient_harness(ledger_path, monkeypatch):
    monkeypatch.setattr(
        "fno.harness_identity.current_session_id", lambda env=None: "ambient-uuid"
    )
    entry = _register.build_quick_entry("", "think", "t")
    assert entry["sessions"] == ["ambient-uuid"]
    entry = _register.build_quick_entry("caller-1", "plan", "t")
    assert entry["sessions"] == ["caller-1", "ambient-uuid"]


def test_quick_entry_marks_when_no_id_anywhere(ledger_path, monkeypatch):
    monkeypatch.setattr("fno.harness_identity.current_session_id", lambda env=None: None)
    entry = _register.build_quick_entry("", "think", "t")
    assert entry["sessions"] == [LEDGER_SESSION_UNRESOLVED]


def test_upsert_backstop_row_carries_the_node_sessions(ledger_path):
    _write_ledger(ledger_path, [])
    url = "https://github.com/o/r/pull/507"
    _register.upsert_ledger_pr(
        "x-3344", 507, url, "r", "2026-08-31T00:00:00Z", node_sessions=["u1", "u2"]
    )
    row = json.loads(ledger_path.read_text())["entries"][-1]
    assert row["sessions"] == ["u1", "u2"]


def test_upsert_backstop_row_marks_when_node_has_none(ledger_path):
    _write_ledger(ledger_path, [])
    _register.upsert_ledger_pr(
        "x-5566", 508, "https://github.com/o/r/pull/508", "r", "2026-09-02T22:43:24Z"
    )
    row = json.loads(ledger_path.read_text())["entries"][-1]
    assert row["sessions"] == [LEDGER_SESSION_UNRESOLVED]
    # x-b6bd: the backstop row's completed carries the one UTC shape too.
    assert row["completed"] == "2026-09-02T22:43:24+00:00"


# --- change 2: the harvest ---------------------------------------------------


def test_harvest_fills_and_marks_then_is_idempotent(ledger_path):
    rows = [
        {"type": "execution", "graph_node_id": "x-3344", "pr_number": 1},
        {"type": "execution", "graph_node_id": "x-5566", "pr_number": 2},
        {"type": "execution", "graph_node_id": "x-7788", "sessions": ["keep"]},
        {"cost_usd": 0.5},  # cost_backfill shape: no type, out of scope
    ]
    _write_ledger(ledger_path, rows)
    nodes = {
        "x-3344": {
            "sessions": [
                {"session_id": "u1", "harness": "claude"},
                {"session_id": "u2", "harness": "codex"},
                "junk",
                {"harness": "codex"},
            ]
        },
        "x-5566": {},
    }
    before = ledger_path.read_bytes()
    assert _register.harvest_ledger_sessions(nodes, dry_run=False) == (1, 1)
    data = json.loads(ledger_path.read_text())["entries"]
    assert data[0]["sessions"] == ["u1", "u2"]
    assert data[1]["sessions"] == [LEDGER_SESSION_UNRESOLVED]
    assert data[2]["sessions"] == ["keep"]
    assert "sessions" not in data[3]
    # Second pass changes nothing - byte-identical.
    assert _register.harvest_ledger_sessions(nodes, dry_run=False) == (0, 0)
    assert ledger_path.read_bytes() != before
    after_first = ledger_path.read_bytes()
    assert _register.harvest_ledger_sessions(nodes, dry_run=False) == (0, 0)
    assert ledger_path.read_bytes() == after_first


def test_harvest_dry_run_reports_counts_without_writing(ledger_path):
    _write_ledger(
        ledger_path,
        [{"type": "execution", "graph_node_id": "x-3344", "pr_number": 1}],
    )
    nodes = {"x-3344": {"sessions": [{"session_id": "u1"}]}}
    before = ledger_path.read_bytes()
    stat_before = ledger_path.stat().st_mtime_ns
    assert _register.harvest_ledger_sessions(nodes, dry_run=True) == (1, 0)
    assert ledger_path.read_bytes() == before
    assert ledger_path.stat().st_mtime_ns == stat_before


# --- change 4: the marker never leaks out as a session id --------------------


def test_observer_first_session_id_skips_the_marker():
    from fno.observer.fold import _first_session_id

    assert _first_session_id({"sessions": [LEDGER_SESSION_UNRESOLVED]}) is None
    assert _first_session_id({"sessions": [UUID]}) == UUID
    assert _first_session_id({}) is None


def test_resolve_pr_sessions_skips_the_marker(tmp_path):
    _write_ledger(
        tmp_path / "ledger.json",
        [
            {
                "pr_number": 507,
                "pr_url": "https://github.com/o/r/pull/507",
                "sessions": [LEDGER_SESSION_UNRESOLVED],
            }
        ],
    )
    ids, reason = resolve_pr_sessions(tmp_path / "ledger.json", 507, "o/r")
    assert ids == []
    assert reason == "no ledger entry for PR #507"
    assert LEDGER_SESSION_UNRESOLVED not in ids


def test_resolve_pr_sessions_returns_only_real_ids(tmp_path):
    _write_ledger(
        tmp_path / "ledger.json",
        [
            {
                "pr_number": 507,
                "pr_url": "https://github.com/o/r/pull/507",
                "sessions": [UUID, LEDGER_SESSION_UNRESOLVED],
            }
        ],
    )
    ids, reason = resolve_pr_sessions(tmp_path / "ledger.json", 507, "o/r")
    assert ids == [UUID]
    assert reason is None
