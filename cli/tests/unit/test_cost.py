"""Tests for fno.cost - ledger + budget integration."""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from unittest.mock import MagicMock, patch


# ---- Helpers ----

def _make_ledger(tmp_path: Path) -> Path:
    """Create a minimal ledger.json."""
    fno_dir = tmp_path / ".fno"
    fno_dir.mkdir(parents=True, exist_ok=True)
    ledger_path = fno_dir / "ledger.json"
    ledger_path.write_text("[]")
    return ledger_path


def _make_graph(tmp_path: Path, node_id: str = "ab-12345678") -> Path:
    """Create a minimal graph.json with one node in the canonical {'entries': [...]} envelope."""
    fno_dir = tmp_path / ".fno"
    fno_dir.mkdir(parents=True, exist_ok=True)
    graph_path = fno_dir / "graph.json"
    node = {
        "id": node_id,
        "title": "Test Feature",
        "cost_usd": None,
        "cost_sessions": [],
    }
    graph_path.write_text(json.dumps({"entries": [node]}))
    return graph_path


# ---- AC1-HP: cost.update appends to ledger ----

def _read_entries(ledger_path: Path) -> list[dict]:
    """Read ledger entries from either the canonical `{"entries": [...]}`
    envelope or a legacy flat-list ledger. Mirrors `_append_to_ledger`'s
    read-side tolerance so the tests don't bind to one shape.
    """
    raw = json.loads(ledger_path.read_text())
    if isinstance(raw, list):
        return raw
    if isinstance(raw, dict):
        entries = raw.get("entries", [])
        return entries if isinstance(entries, list) else []
    return []


def test_ac1_hp_cost_update_appends_to_ledger(tmp_path):
    """cost.update() appends an entry to ledger.json for the session."""
    ledger_path = _make_ledger(tmp_path)

    from fno.cost import update as cost_update

    cost_update(
        session_id="20260421T160000Z-55555-bb2233",
        tokens=1000,
        cost_usd=0.50,
        ledger_path=ledger_path,
    )

    entries = _read_entries(ledger_path)
    assert len(entries) == 1
    entry = entries[0]
    assert entry["session_id"] == "20260421T160000Z-55555-bb2233"
    assert entry["cost_usd"] == 0.50
    assert entry["tokens"] == 1000


def test_ac1_hp_cost_update_appends_multiple(tmp_path):
    """Multiple cost.update() calls each append a separate entry."""
    ledger_path = _make_ledger(tmp_path)

    from fno.cost import update as cost_update

    cost_update("sess-001", tokens=500, cost_usd=0.25, ledger_path=ledger_path)
    cost_update("sess-002", tokens=800, cost_usd=0.40, ledger_path=ledger_path)

    entries = _read_entries(ledger_path)
    assert len(entries) == 2
    assert entries[0]["session_id"] == "sess-001"
    assert entries[1]["session_id"] == "sess-002"


# ---- AC1-HP: cost.update appends to graph node's cost_sessions ----

def test_ac1_hp_cost_update_appends_to_graph_node(tmp_path):
    """cost.update() appends to the graph node's cost_sessions[] when node_id provided."""
    ledger_path = _make_ledger(tmp_path)
    graph_path = _make_graph(tmp_path, "ab-12345678")

    from fno.cost import update as cost_update

    cost_update(
        session_id="sess-graph-test",
        tokens=2000,
        cost_usd=1.00,
        ledger_path=ledger_path,
        graph_path=graph_path,
        node_id="ab-12345678",
    )

    graph = json.loads(graph_path.read_text())
    assert "entries" in graph, "graph.json must preserve the {'entries': [...]} envelope"
    node = graph["entries"][0]
    assert len(node["cost_sessions"]) == 1
    assert node["cost_sessions"][0]["session_id"] == "sess-graph-test"
    assert node["cost_sessions"][0]["cost_usd"] == 1.00
    # Cumulative cost_usd updated
    assert node["cost_usd"] == 1.00


# ---- AC2-HP: budget cap check ----

def test_ac2_hp_budget_cap_blocks_phase(tmp_path):
    """check_budget() returns True (over cap) when total >= cap."""
    from fno.cost import check_budget

    assert check_budget(total_cost_usd=24.50, budget_cap_usd=25.00, estimated_phase_cost=1.00) is True


def test_ac2_hp_budget_cap_allows_phase(tmp_path):
    """check_budget() returns False when total + estimated < cap."""
    from fno.cost import check_budget

    assert check_budget(total_cost_usd=10.00, budget_cap_usd=25.00, estimated_phase_cost=1.00) is False


def test_ac2_hp_budget_cap_none_allows(tmp_path):
    """check_budget() returns False when budget_cap_usd is None (no cap set)."""
    from fno.cost import check_budget

    assert check_budget(total_cost_usd=100.00, budget_cap_usd=None, estimated_phase_cost=1.00) is False


def test_ac2_hp_budget_cap_at_boundary(tmp_path):
    """check_budget() returns True when total + estimated exactly equals cap."""
    from fno.cost import check_budget

    # 24.50 + 0.50 = 25.00 = cap -> over cap
    assert check_budget(total_cost_usd=24.50, budget_cap_usd=25.00, estimated_phase_cost=0.50) is True


# ---- subprocess failure surface (AC1-ERR, AC1-FR, AC1-EDGE, AC1-UI, AC1-HP-no-event) ----

def _make_failed_result(returncode: int, stderr: str) -> subprocess.CompletedProcess:
    """Build a fake subprocess.CompletedProcess representing script failure."""
    r = MagicMock(spec=subprocess.CompletedProcess)
    r.returncode = returncode
    r.stderr = stderr
    r.stdout = ""
    return r


def _events_path(tmp_path: Path) -> Path:
    fno_dir = tmp_path / ".fno"
    fno_dir.mkdir(parents=True, exist_ok=True)
    return fno_dir / "events.jsonl"


def _read_events(events_path: Path) -> list[dict]:
    if not events_path.exists():
        return []
    return [json.loads(line) for line in events_path.read_text().splitlines() if line.strip()]


def test_ac1_hp_no_stderr_no_event_on_success(tmp_path, capsys):
    """AC1-HP: rc=0 path produces no stderr and no cost_subprocess_failed event."""
    ledger_path = _make_ledger(tmp_path)
    events_path = _events_path(tmp_path)

    from fno.cost import update as cost_update

    ok_result = MagicMock(spec=subprocess.CompletedProcess)
    ok_result.returncode = 0
    ok_result.stderr = ""
    ok_result.stdout = ""

    # update() now runs `python3 -m fno.cost._session_cost`; patch subprocess.run
    # to simulate a clean rc=0 so the no-stderr/no-event path is exercised.
    with patch("subprocess.run", return_value=ok_result):
        cost_update(
            session_id="sess-hp-001",
            tokens=100,
            cost_usd=0.01,
            ledger_path=ledger_path,
        )

    captured = capsys.readouterr()
    assert captured.err == "", f"Expected no stderr on rc=0, got: {captured.err!r}"

    events = _read_events(events_path)
    cost_events = [e for e in events if e.get("type") == "cost_subprocess_failed"]
    assert len(cost_events) == 0, f"Expected no cost_subprocess_failed events on rc=0, got: {cost_events}"


def test_ac1_err_subprocess_failure_surfaces_stderr_and_event(tmp_path, capsys):
    """AC1-ERR: rc!=0 with stderr 'boom' -> stderr printed, event emitted, ledger updated."""
    ledger_path = _make_ledger(tmp_path)

    from fno.cost import update as cost_update

    failed_result = _make_failed_result(returncode=2, stderr="boom")

    captured_events = []

    def fake_append(event, path=None, **kw):
        captured_events.append(event)

    with patch("subprocess.run", return_value=failed_result), \
         patch("fno.cost.append_event", side_effect=fake_append):

        cost_update(
            session_id="sess-err-001",
            tokens=200,
            cost_usd=0.10,
            ledger_path=ledger_path,
        )

    captured = capsys.readouterr()
    assert "cost.py: subprocess failed:" in captured.err, \
        f"Expected 'cost.py: subprocess failed:' in stderr, got: {captured.err!r}"
    assert "boom" in captured.err, \
        f"Expected 'boom' in stderr output, got: {captured.err!r}"

    assert len(captured_events) == 1, f"Expected 1 cost_subprocess_failed event, got: {captured_events}"
    evt = captured_events[0]
    assert evt["type"] == "cost_subprocess_failed"
    assert evt["data"]["fallback_succeeded"] is True
    assert evt["data"]["returncode"] == 2
    assert evt["data"]["stderr_snippet"] == "boom"

    entries = _read_entries(ledger_path)
    assert len(entries) == 1, f"Expected 1 ledger entry (direct fallback), got: {entries}"
    assert entries[0]["session_id"] == "sess-err-001"


def test_ac1_fr_unwritable_events_does_not_break_ledger(tmp_path, capsys):
    """AC1-FR: events.jsonl unwritable -> ledger still updated, exit 0, stderr mentions event-emit failure."""
    ledger_path = _make_ledger(tmp_path)

    from fno.cost import update as cost_update

    failed_result = _make_failed_result(returncode=2, stderr="script error")

    def raise_on_append(event, path=None, **kw):
        raise OSError("Permission denied: events.jsonl")

    with patch("subprocess.run", return_value=failed_result), \
         patch("fno.cost.append_event", side_effect=raise_on_append):

        result = cost_update(
            session_id="sess-fr-001",
            tokens=100,
            cost_usd=0.05,
            ledger_path=ledger_path,
        )

    assert result["ok"] is True, "cost_update must return ok=True even when event emit fails"

    entries = _read_entries(ledger_path)
    assert len(entries) == 1, f"Ledger must have 1 entry even when events.jsonl is unwritable, got: {entries}"

    captured = capsys.readouterr()
    # The stderr should mention the event-emit failure
    assert captured.err != "", "Expected some stderr about event-emit failure"


def test_ac1_edge_empty_stderr_event_emitted_no_print(tmp_path, capsys):
    """AC1-EDGE: rc!=0 with empty stderr -> event still emitted with stderr_snippet='', no print."""
    ledger_path = _make_ledger(tmp_path)

    from fno.cost import update as cost_update

    failed_result = _make_failed_result(returncode=2, stderr="")

    captured_events = []

    def fake_append(event, path=None, **kw):
        captured_events.append(event)

    with patch("subprocess.run", return_value=failed_result), \
         patch("fno.cost.append_event", side_effect=fake_append):

        cost_update(
            session_id="sess-edge-001",
            tokens=50,
            cost_usd=0.02,
            ledger_path=ledger_path,
        )

    captured = capsys.readouterr()
    assert captured.err == "", \
        f"Expected no stderr when subprocess stderr is empty, got: {captured.err!r}"

    assert len(captured_events) == 1, f"Expected event even with empty stderr, got: {captured_events}"
    evt = captured_events[0]
    assert evt["data"]["stderr_snippet"] == ""
    assert evt["data"]["returncode"] == 2


def test_ac1_ui_stderr_prefix_exact(tmp_path, capsys):
    """AC1-UI: stderr line is prefixed exactly 'cost.py: subprocess failed:' (load-bearing for log scanners)."""
    ledger_path = _make_ledger(tmp_path)

    from fno.cost import update as cost_update

    failed_result = _make_failed_result(returncode=1, stderr="non-empty error output")

    with patch("subprocess.run", return_value=failed_result), \
         patch("fno.cost.append_event"):

        cost_update(
            session_id="sess-ui-001",
            tokens=100,
            cost_usd=0.05,
            ledger_path=ledger_path,
        )

    captured = capsys.readouterr()
    stderr_lines = [line for line in captured.err.splitlines() if line.strip()]
    assert any(line.startswith("cost.py: subprocess failed:") for line in stderr_lines), \
        f"Expected a line starting with 'cost.py: subprocess failed:' in stderr, got lines: {stderr_lines}"


# ---------------------------------------------------------------------------
# AC1-FR-OSE: subprocess.run raises OSError before completing.
# The ledger fallback MUST still write and a forensic event MUST be emitted
# with returncode=-1 and the exception text as stderr_snippet. Without the
# try/except around subprocess.run, the OSError propagates out of update()
# and the fallback never runs (silent failure - the very class this sweep
# is meant to eliminate).
# ---------------------------------------------------------------------------

def test_ac1_fr_subprocess_oserror_still_writes_ledger_and_event(tmp_path, capsys):
    """AC1-FR-OSE: when subprocess.run raises OSError (text-file-busy,
    permission denied on sys.executable, etc.), the ledger fallback still
    runs and a cost_subprocess_failed event is emitted with returncode=-1."""
    ledger_path = _make_ledger(tmp_path)

    from fno.cost import update as cost_update

    captured_events: list[dict] = []

    def fake_append(event, path=None, **kw):
        captured_events.append(event)

    def _raise_oserror(*args, **kwargs):
        raise OSError("text file busy")

    with patch("subprocess.run", side_effect=_raise_oserror), \
         patch("fno.cost.append_event", side_effect=fake_append):

        cost_update(
            session_id="sess-ose-001",
            tokens=10,
            cost_usd=0.002,
            ledger_path=ledger_path,
        )

    # Ledger fallback ran despite OSError on subprocess.run.
    entries = _read_entries(ledger_path)
    assert len(entries) == 1, f"Expected 1 ledger entry from fallback, got: {entries}"
    assert entries[0]["session_id"] == "sess-ose-001"

    # Forensic event was emitted with the OSError text in stderr_snippet.
    cost_events = [e for e in captured_events if e.get("type") == "cost_subprocess_failed"]
    assert len(cost_events) == 1, f"Expected 1 cost_subprocess_failed event, got: {cost_events}"
    assert cost_events[0]["data"]["returncode"] == -1
    assert "text file busy" in cost_events[0]["data"]["stderr_snippet"]
    assert cost_events[0]["data"]["fallback_succeeded"] is True


# ---------------------------------------------------------------------------
# AC1-EDGE-MB: 4KB byte cap holds even when stderr contains multi-byte UTF-8
# already over the cap and the slice lands mid-codepoint. Using errors="replace"
# inserts U+FFFD (3 bytes encoded) for partial codepoints and can push the
# re-encoded snippet past the cap; errors="ignore" drops partial bytes so
# the result stays under the budget. Also pins that the suffix is included
# in the budget rather than appended outside it.
# ---------------------------------------------------------------------------

def test_ac1_edge_multibyte_stderr_respects_4kb_byte_cap(tmp_path, capsys):
    """AC1-EDGE-MB: multi-byte UTF-8 stderr already over the cap must
    truncate so the re-encoded snippet (including the [...truncated] suffix)
    is <= 4096 bytes, even when the slice lands mid-codepoint."""
    ledger_path = _make_ledger(tmp_path)

    from fno.cost import update as cost_update

    # Heart-emoji-with-VS16 is 6 bytes UTF-8; 1000 of them is 6000 bytes,
    # well past the 4096 cap. Slice at 4081 bytes will land mid-codepoint.
    long_stderr = "❤️" * 1000  # 6000 bytes UTF-8

    failed_result = _make_failed_result(returncode=2, stderr=long_stderr)

    captured_events: list[dict] = []

    def fake_append(event, path=None, **kw):
        captured_events.append(event)

    with patch("subprocess.run", return_value=failed_result), \
         patch("fno.cost.append_event", side_effect=fake_append):

        cost_update(
            session_id="sess-mb-001",
            tokens=10,
            cost_usd=0.001,
            ledger_path=ledger_path,
        )

    cost_events = [e for e in captured_events if e.get("type") == "cost_subprocess_failed"]
    assert len(cost_events) == 1
    snippet = cost_events[0]["data"]["stderr_snippet"]
    snippet_bytes = len(snippet.encode("utf-8"))
    assert snippet_bytes <= 4096, (
        f"stderr_snippet exceeded 4KB byte cap: {snippet_bytes} bytes "
        f"(suffix must be inside the budget, not appended outside)"
    )
    assert snippet.endswith("[...truncated]"), \
        f"Truncation marker missing; got snippet ending: {snippet[-20:]!r}"
