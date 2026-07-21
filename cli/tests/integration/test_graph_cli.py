"""Integration tests for `fno graph` subcommands via the typer CLI.

Each test verifies behavior matches the legacy roadmap-tasks.py script.
Uses typer.testing.CliRunner for speed; the FNO_GRAPH_JSON env var
routes all graph I/O to a temp file so the real ~/.fno/graph.json
is never touched.
"""
from __future__ import annotations

import json
import os
import tempfile
import unittest.mock
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()

REPO_ROOT = Path(__file__).parent.parent.parent.parent


def _write_plan(dirpath: Path, name: str, title: str) -> Path:
    p = dirpath / name
    p.write_text(f"---\ntitle: {title}\n---\n# {title}\n")
    return p


def _recent_iso(days_ago: int = 1) -> str:
    """A created_at within the G1 stale-ready window (x-3236). Ordering fixtures
    that predate the guard used fixed 2026-01 dates that now read as abandoned;
    selection tests must anchor to 'now' so an incidental old date does not
    quarantine a node under test. Relative order is preserved via days_ago."""
    from datetime import datetime, timezone, timedelta

    return (datetime.now(timezone.utc) - timedelta(days=days_ago)).isoformat()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    """A fresh empty graph.json; monkeypatches fno.graph constants to use it."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    # Patch the module-level constants so all operations hit this temp file
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    # Also patch the store module's imported names
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


def _invoke(*args, input=None):
    """Invoke the fno CLI and return the result."""
    return runner.invoke(app, list(args), input=input, catch_exceptions=False)


def _read_graph(g: Path) -> list[dict]:
    if not g.exists():
        return []
    return json.loads(g.read_text()).get("entries", [])


# --- x-30f6: ambient provenance stamp at node birth ---

def test_ac_hp_idea_stamps_ambient_session(tmp_graph, tmp_path, monkeypatch):
    """AC-HP (x-30f6 2.1): `idea` stamps source_session_id + harness from env, no flag."""
    for var in (
        "CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID"
    ):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "itest-sess-7")
    # cwd without an owned manifest -> session+harness stamped, node/plan null.
    monkeypatch.chdir(tmp_path)

    r = _invoke("graph", "idea", "Ambient idea")
    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    assert entries[0]["source_session_id"] == "itest-sess-7"
    assert entries[0]["source_harness"] == "claude"


def test_ac_edge_idea_no_env_null_provenance(tmp_graph, tmp_path, monkeypatch):
    """AC-EDGE (x-30f6 2.1): no env -> provenance fields persist as null, no error."""
    for var in (
        "CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID"
    ):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.chdir(tmp_path)

    r = _invoke("graph", "idea", "Quiet idea")
    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    assert entries[0]["source_session_id"] is None
    assert entries[0]["source_harness"] is None
    assert entries[0]["source_node_id"] is None


# --- add ---

def test_ac1_hp_graph_add(tmp_graph):
    """AC1-HP: fno graph add creates a node and returns JSON."""
    r = _invoke("graph", "add", "My Feature")
    assert r.exit_code == 0, r.output
    data = json.loads(r.output)
    assert data["id"].startswith("ab-")
    assert data["title"] == "My Feature"


def test_ac1_hp_graph_add_with_priority(tmp_graph):
    """AC1-HP: fno graph add --priority p1 is respected."""
    r = _invoke("graph", "add", "High Priority", "--priority", "p1")
    assert r.exit_code == 0, r.output
    data = json.loads(r.output)
    assert data["id"].startswith("ab-")
    entries = _read_graph(tmp_graph)
    assert entries[0]["priority"] == "p1"


def test_ac2_err_graph_add_invalid_priority(tmp_graph):
    """AC2-ERR: fno graph add with invalid priority exits 1."""
    r = runner.invoke(app, ["graph", "add", "Bad", "--priority", "urgent"], catch_exceptions=True)
    assert r.exit_code != 0


def test_configured_prefix_mint_and_resolve(tmp_graph, monkeypatch):
    """ab-bbfccb8f end-to-end: a configured prefix/width mints configured-format
    ids (US2: ``xy-`` + 4 hex) that then resolve through the CLI verbs (US4:
    ``update`` would have hard-errored under the old ``startswith('ab-')`` gate)."""
    import re

    from fno.config import SettingsModel

    model = SettingsModel(config={"backlog": {"id_prefix": "xy-", "id_hex_width": 4}})
    monkeypatch.setattr("fno.config.load_settings", lambda: model)

    r = _invoke("graph", "add", "Configured Feature")
    assert r.exit_code == 0, r.output
    nid = json.loads(r.output)["id"]
    assert re.fullmatch(r"xy-[0-9a-f]{4}", nid), nid

    r2 = _invoke("graph", "update", nid, "--priority", "p1")
    assert r2.exit_code == 0, r2.output
    entries = _read_graph(tmp_graph)
    assert entries[0]["priority"] == "p1"


def test_update_model_tier_valid_writes_node(tmp_graph):
    r = _invoke("graph", "add", "Tiered Feature")
    nid = json.loads(r.output)["id"]
    r2 = _invoke("graph", "update", nid, "--model-tier", "LOW")
    assert r2.exit_code == 0, r2.output
    assert _read_graph(tmp_graph)[0]["model_tier"] == "low"  # normalized


def test_update_model_tier_invalid_exits_2(tmp_graph):
    r = _invoke("graph", "add", "Tiered Feature")
    nid = json.loads(r.output)["id"]
    r2 = runner.invoke(app, ["graph", "update", nid, "--model-tier", "turbo"])
    assert r2.exit_code == 2
    assert "invalid --model-tier" in r2.output
    assert "model_tier" not in _read_graph(tmp_graph)[0]


def test_update_model_tier_null_clears(tmp_graph):
    r = _invoke("graph", "add", "Tiered Feature")
    nid = json.loads(r.output)["id"]
    _invoke("graph", "update", nid, "--model-tier", "high")
    _invoke("graph", "update", nid, "--model-tier", "null")
    assert _read_graph(tmp_graph)[0]["model_tier"] is None


def test_pick_extract_id_ignores_prefixed_non_id_tokens(monkeypatch):
    """gemini HIGH: picker extraction must use the strict matcher so a token
    that merely starts with the prefix (e.g. a project name `fno-cli`) is not
    misread as the node id."""
    from fno.config import SettingsModel
    from fno.graph.cli import _pick_extract_id

    model = SettingsModel(config={"backlog": {"id_prefix": "fno-", "id_hex_width": 4}})
    monkeypatch.setattr("fno.config.load_settings", lambda: model)

    # A TSV-ish row: project name (prefix-shaped but not an id) then the real id.
    assert _pick_extract_id("fno-cli\tSome title\tfno-a3f9") == "fno-a3f9"
    # No well-formed id present -> None (the project name is not extracted).
    assert _pick_extract_id("fno-cli\tSome title") is None


def test_legacy_id_resolves_under_configured_install(tmp_graph, monkeypatch):
    """AC3-HP/AC3-EDGE: a configured install still resolves a historical ab- id
    (mixed-format graph), because resolution honors both the configured and the
    legacy prefix."""
    # Seed a legacy 8-hex node directly.
    g = tmp_graph
    data = json.loads(g.read_text())
    data["entries"].append({
        "id": "ab-55ba9adb", "title": "Legacy node", "priority": "p2",
        "status": "ready", "blocked_by": [], "type": "feature",
    })
    g.write_text(json.dumps(data))

    from fno.config import SettingsModel
    model = SettingsModel(config={"backlog": {"id_prefix": "xy-", "id_hex_width": 4}})
    monkeypatch.setattr("fno.config.load_settings", lambda: model)

    r = _invoke("graph", "update", "ab-55ba9adb", "--priority", "p0")
    assert r.exit_code == 0, r.output
    entries = _read_graph(g)
    assert entries[0]["priority"] == "p0"


# --- next ---

def test_ac1_hp_graph_next_empty(tmp_graph):
    """AC1-HP: fno graph next on empty graph returns null."""
    r = _invoke("graph", "next", "--all")
    assert r.exit_code == 0, r.output
    assert r.output.strip() == "null"


def test_ac1_hp_graph_next_returns_highest_priority(tmp_graph):
    """AC1-HP: fno graph next picks highest priority.

    `graph add` creates plan-less nodes (idea status), so `next` needs
    `--include-ideas` to consider them. The default exclusion behavior
    is covered separately in test_graph_status.py.
    """
    _invoke("graph", "add", "Low", "--priority", "p3")
    _invoke("graph", "add", "High", "--priority", "p1")
    r = _invoke("graph", "next", "--all", "--include-ideas")
    assert r.exit_code == 0
    data = json.loads(r.output)
    assert data["title"] == "High"


# --- ready ---

def test_ac1_hp_graph_ready_returns_json_array(tmp_graph):
    """AC1-HP: fno graph ready returns JSON array.

    `graph add` creates plan-less idea-stage nodes; `--include-ideas`
    surfaces them in the listing. The default exclusion behavior is
    covered separately in test_graph_status.py.
    """
    _invoke("graph", "add", "Feature 1")
    r = _invoke("graph", "ready", "--all", "--include-ideas")
    assert r.exit_code == 0
    data = json.loads(r.output)
    assert isinstance(data, list)
    assert len(data) == 1
    assert data[0]["title"] == "Feature 1"


# --- get ---

def test_ac1_hp_graph_get_returns_node(tmp_graph):
    """AC1-HP: fno graph get returns full node JSON."""
    r = _invoke("graph", "add", "GetTarget")
    node_id = json.loads(r.output)["id"]

    r = _invoke("graph", "get", node_id)
    assert r.exit_code == 0
    data = json.loads(r.output)
    assert data["id"] == node_id
    assert data["title"] == "GetTarget"


def test_ac2_err_graph_get_unknown_exits_nonzero(tmp_graph):
    """AC2-ERR: fno graph get unknown ID exits 1."""
    r = runner.invoke(app, ["graph", "get", "ab-deadbeef"], catch_exceptions=True)
    assert r.exit_code != 0


# --- update ---

def test_ac6_edge_graph_update_completed_removed(tmp_graph):
    """AC6-EDGE: `update --completed` is gone and fails loudly.

    It was an ungated, event-silent close. Closing is merge-gated and belongs
    to done/reconcile, so the flag must not merely be ignored - a silently
    accepted no-op would let a caller believe it closed the node.
    """
    r = _invoke("graph", "add", "ToDone")
    node_id = json.loads(r.output)["id"]

    r = _invoke("graph", "update", node_id, "--completed")
    assert r.exit_code != 0

    r = _invoke("graph", "get", node_id)
    data = json.loads(r.output)
    assert data["completed_at"] is None
    assert data["status"] != "done"


# --- queue / unqueue / queued ---

def test_queue_sets_queued_at_and_keeps_ready_status(tmp_graph):
    """Queuing a ready node sets queued_at but does NOT change status (still ready)."""
    r = _invoke("graph", "add", "QueueTarget")
    nid = json.loads(r.output)["id"]
    r = _invoke("graph", "queue", nid, "--reason", "today's focus")
    assert r.exit_code == 0, r.output
    r = _invoke("graph", "get", nid)
    data = json.loads(r.output)
    assert data["queued_at"] is not None
    assert data["queued_reason"] == "today's focus"
    # Status stays ready - queued is orthogonal.
    # (an idea-status node would still be idea; this one has no plan so it's idea)
    assert data["status"] in ("ready", "idea")


def test_unqueue_clears_fields_and_warns_if_not_queued(tmp_graph):
    r = _invoke("graph", "add", "UnqueueTarget")
    nid = json.loads(r.output)["id"]
    _invoke("graph", "queue", nid)
    r = _invoke("graph", "unqueue", nid)
    assert r.exit_code == 0
    data = json.loads(_invoke("graph", "get", nid).output)
    assert data["queued_at"] is None
    assert data["queued_reason"] is None
    # idempotent: unqueue-when-not-queued warns but doesn't error
    r2 = _invoke("graph", "unqueue", nid)
    assert r2.exit_code == 0


def test_queued_lister_filters_by_queued_at(tmp_graph):
    """fno backlog queued lists only nodes with queued_at set."""
    ids = []
    for title in ("A", "B", "C"):
        r = _invoke("graph", "add", title)
        ids.append(json.loads(r.output)["id"])
    _invoke("graph", "queue", ids[0])
    _invoke("graph", "queue", ids[2])
    r = _invoke("graph", "queued")
    listed = {x["id"] for x in json.loads(r.output)}
    assert listed == {ids[0], ids[2]}


def test_pick_format_and_id_extract_round_trip():
    """The picker's row format must round-trip through _pick_extract_id."""
    from fno.graph.cli import _pick_format_line, _pick_extract_id
    e = {
        "id": "ab-abcd1234",
        "title": "Some plan",
        "priority": "p1",
        "project": "fno",
        "plan_path": "internal/fno/plans/2026-05-12-something.md",
        "queued_at": None,
    }
    line = _pick_format_line(e)
    assert "ab-abcd1234" in line
    assert "[ ]" in line and "[Q]" not in line
    # Plan kind column visible
    assert " plan " in line
    assert _pick_extract_id(line) == "ab-abcd1234"
    # Queued marker variant
    e2 = {**e, "queued_at": "2026-05-12T12:00:00Z"}
    assert "[Q]" in _pick_format_line(e2)
    # Defensive: an empty / id-less line returns None
    assert _pick_extract_id("not a real row") is None


def test_pick_format_kind_column_distinguishes_plan_and_idea():
    """Rows with plan_path say 'plan'; rows without say 'idea'."""
    from fno.graph.cli import _pick_format_line
    plan = {
        "id": "ab-abcd1234",
        "title": "x",
        "priority": "p2",
        "project": "fno",
        "plan_path": "internal/fno/plans/2026-05-12-x.md",
    }
    idea = {
        "id": "ab-efgh5678",
        "title": "y",
        "priority": "p2",
        "project": "fno",
        "plan_path": None,
    }
    assert " plan " in _pick_format_line(plan)
    assert " idea " in _pick_format_line(idea)


def test_pick_format_blocked_marker_and_inline_blockers():
    """Blocked rows get [B] marker and inline 'blocked by ...' suffix."""
    from fno.graph.cli import _pick_format_line
    blocker = {"id": "ab-block1234", "completed_at": None}
    e = {
        "id": "ab-dep11111",
        "title": "Depends on the blocker",
        "priority": "p2",
        "project": "fno",
        "status": "blocked",
        "blocked_by": ["ab-block1234"],
        "queued_at": None,
    }
    id_to_entry = {blocker["id"]: blocker, e["id"]: e}
    line = _pick_format_line(e, id_to_entry)
    assert "[B]" in line
    assert "blocked by ab-block1234" in line


def test_pick_format_queued_blocked_combined_marker():
    """A queued + blocked node uses the [Q!] marker."""
    from fno.graph.cli import _pick_format_line
    blocker = {"id": "ab-block2222", "completed_at": None}
    e = {
        "id": "ab-dep22222",
        "title": "Queued and waiting",
        "priority": "p1",
        "project": "fno",
        "status": "blocked",
        "blocked_by": ["ab-block2222"],
        "queued_at": "2026-05-12T12:00:00Z",
    }
    line = _pick_format_line(e, {blocker["id"]: blocker, e["id"]: e})
    assert "[Q!]" in line
    assert "blocked by ab-block2222" in line


def test_pick_truncates_long_project_and_title():
    """Long fields stay readable in fzf rows."""
    from fno.graph.cli import _pick_format_line
    e = {
        "id": "ab-eeeeffff",
        "title": "x" * 200,
        "priority": "p2",
        "project": "y" * 50,
        "queued_at": None,
    }
    line = _pick_format_line(e)
    assert len(line.split("ab-eeeeffff")[1]) <= 95  # title chunk capped
    # project chunk is the third whitespace-aligned column; ensure overall
    # width stays sane.
    assert len(line) < 200


def test_queue_accepts_multiple_ids_space_and_comma_separated(tmp_graph):
    """fno backlog queue ab-X,ab-Y ab-Z queues all three atomically."""
    ids = []
    for title in ("Multi-A", "Multi-B", "Multi-C"):
        r = _invoke("graph", "add", title)
        ids.append(json.loads(r.output)["id"])
    # Mix comma and space separators.
    r = _invoke("graph", "queue", f"{ids[0]},{ids[1]}", ids[2], "--reason", "batch")
    assert r.exit_code == 0, r.output
    queued_ids = {x["id"] for x in json.loads(_invoke("graph", "queued").output)}
    assert queued_ids == set(ids)
    # Same reason on all three.
    for tid in ids:
        data = json.loads(_invoke("graph", "get", tid).output)
        assert data["queued_reason"] == "batch"


def test_queue_batch_is_atomic_on_unknown_id(tmp_graph):
    """If any ID is unknown, no nodes are queued."""
    r = _invoke("graph", "add", "Real")
    real_id = json.loads(r.output)["id"]
    r = _invoke("graph", "queue", f"{real_id},ab-deadbeef")
    assert r.exit_code != 0
    # Real node was NOT queued because the batch aborted.
    data = json.loads(_invoke("graph", "get", real_id).output)
    assert data["queued_at"] is None


def test_unqueue_accepts_multiple_ids(tmp_graph):
    ids = []
    for title in ("UnqA", "UnqB"):
        r = _invoke("graph", "add", title)
        ids.append(json.loads(r.output)["id"])
    _invoke("graph", "queue", ids[0])
    _invoke("graph", "queue", ids[1])
    r = _invoke("graph", "unqueue", f"{ids[0]},{ids[1]}")
    assert r.exit_code == 0
    queued_listing = json.loads(_invoke("graph", "queued").output)
    assert queued_listing == []


def test_done_clears_queued_state(tmp_graph):
    r = _invoke("graph", "add", "QueuedThenDone")
    nid = json.loads(r.output)["id"]
    _invoke("graph", "queue", nid)
    _invoke("graph", "done", nid)
    data = json.loads(_invoke("graph", "get", nid).output)
    assert data["queued_at"] is None
    assert data["completed_at"] is not None


def test_done_audit_tags_operator_when_driving(tmp_graph, monkeypatch):
    """cv-9def52a7: `done` during a drive window emits backlog_done_operator_initiated."""
    from fno.agents import drive_authority as da

    captured: dict = {}
    monkeypatch.setattr(da, "is_drive_authority_active", lambda *a, **k: True)
    monkeypatch.setattr(
        da,
        "emit_operator_initiated",
        lambda action_type, **kw: captured.update(type=action_type, kw=kw),
    )
    nid = json.loads(_invoke("graph", "add", "DriveDone").output)["id"]
    _invoke("graph", "done", nid)
    assert captured.get("type") == "backlog_done_operator_initiated"
    assert captured["kw"]["task_id"] == nid
    assert captured["kw"]["source"] == "backlog"


def test_done_no_audit_tag_when_not_driving(tmp_graph, monkeypatch):
    """No drive window -> done does not emit the operator-initiated tag."""
    from fno.agents import drive_authority as da

    calls = {"n": 0}
    monkeypatch.setattr(da, "is_drive_authority_active", lambda *a, **k: False)
    monkeypatch.setattr(
        da, "emit_operator_initiated", lambda *a, **k: calls.update(n=calls["n"] + 1)
    )
    nid = json.loads(_invoke("graph", "add", "NoDriveDone").output)["id"]
    _invoke("graph", "done", nid)
    assert calls["n"] == 0


# --- view ---

def test_ac1_hp_graph_view_renders_html_and_prints_path(tmp_graph, tmp_path, monkeypatch):
    """AC1-HP: fno graph view rerenders HTML and echoes the path."""
    monkeypatch.setenv("FNO_NO_OPEN", "1")
    html_path = tmp_path / "graph.html"

    _invoke("graph", "add", "ViewTarget")
    r = _invoke("graph", "view")
    assert r.exit_code == 0, r.output
    assert str(html_path) in r.output
    assert html_path.exists()
    text = html_path.read_text(encoding="utf-8")
    assert "ViewTarget" in text
    assert "<html" in text


def test_ac2_err_graph_view_empty_graph_still_renders(tmp_graph, tmp_path, monkeypatch):
    """AC2-ERR: view on an empty graph produces an HTML shell, not an error."""
    monkeypatch.setenv("FNO_NO_OPEN", "1")
    html_path = tmp_path / "graph.html"

    r = _invoke("graph", "view")
    assert r.exit_code == 0, r.output
    assert html_path.exists()
    assert "<html" in html_path.read_text(encoding="utf-8")


def test_no_test_leaks_to_real_graph_html(tmp_graph, tmp_path):
    """Regression guard: mutations under tmp_graph must NOT touch ~/.fno/graph.html.

    The tmp_graph fixture patches gc.GRAPH_HTML; render_graph_html resolves
    its default path from gc lazily. If either side regresses, this test
    starts writing to the user's real backlog file.
    """
    real_path = Path.home() / ".fno" / "graph.html"
    before_mtime = real_path.stat().st_mtime if real_path.exists() else None
    _invoke("graph", "add", "LeakCanary")
    after_mtime = real_path.stat().st_mtime if real_path.exists() else None
    assert before_mtime == after_mtime, (
        f"tmp_graph fixture leaked to {real_path} - mutations under the "
        f"fixture must not touch the real user backlog."
    )


# --- tree ---

def test_ac1_hp_graph_tree(tmp_graph):
    """AC1-HP: fno graph tree shows output."""
    _invoke("graph", "add", "Root Feature")
    r = _invoke("graph", "tree")
    assert r.exit_code == 0
    assert "Root Feature" in r.output


# --- status ---

def test_ac1_hp_graph_status(tmp_graph):
    """AC1-HP: fno graph status shows progress summary."""
    _invoke("graph", "add", "Feature A", "--project", "test-proj")
    r = _invoke("graph", "status", "--all")
    assert r.exit_code == 0
    assert "test-proj" in r.output


# --- validate ---

def test_ac1_hp_graph_validate_clean(tmp_graph):
    """AC1-HP: fno graph validate on clean graph exits 0."""
    _invoke("graph", "add", "Clean")
    r = _invoke("graph", "validate")
    assert r.exit_code == 0
    assert "OK" in r.output or "no issues" in r.output.lower()


# --- cost ---

def test_ac1_hp_graph_cost(tmp_graph):
    """AC1-HP: fno graph cost records session cost (#23).

    Replaces the substring-on-stdout assertion with a state round-trip
    through `graph get`. A CLI text-format regression should not mask
    a missing or wrong-value cost write.
    """
    r = _invoke("graph", "add", "Costly")
    node_id = json.loads(r.output)["id"]

    r = _invoke("graph", "cost", node_id, "--session", "sess-001", "--amount", "1.50")
    assert r.exit_code == 0

    # State round-trip (#23): the cost write must be visible via
    # `graph get`. The cost_usd field aggregates across sessions and
    # cost_sessions records the individual session attribution.
    r = _invoke("graph", "get", node_id)
    data = json.loads(r.output)
    assert data["cost_usd"] == pytest.approx(1.50)
    cost_sessions = data.get("cost_sessions") or []
    assert any(s.get("session_id") == "sess-001" for s in cost_sessions), (
        f"sess-001 should appear in cost_sessions, got {cost_sessions!r}"
    )


# --- briefs ---

def test_ac1_hp_graph_briefs_empty(tmp_graph):
    """AC1-HP: fno graph briefs returns JSON array."""
    r = _invoke("graph", "briefs")
    assert r.exit_code == 0
    data = json.loads(r.output)
    assert isinstance(data, list)


# --- remove ---

def test_ac1_hp_graph_remove(tmp_graph):
    """AC1-HP: fno graph remove deletes a node."""
    r = _invoke("graph", "add", "ToRemove")
    node_id = json.loads(r.output)["id"]

    r = _invoke("graph", "remove", node_id, "--force")
    assert r.exit_code == 0

    r = runner.invoke(app, ["graph", "get", node_id], catch_exceptions=True)
    assert r.exit_code != 0


# --- defer ---

def test_ac1_hp_graph_defer(tmp_graph):
    """AC1-HP: fno graph defer sets deferred_at + deferred_reason and derives status: deferred."""
    r = _invoke("graph", "add", "ToDefer")
    node_id = json.loads(r.output)["id"]

    r = _invoke("graph", "defer", node_id, "--reason", "stale spec")
    assert r.exit_code == 0

    r = _invoke("graph", "get", node_id)
    data = json.loads(r.output)
    assert data.get("deferred_at"), "deferred_at should be set to an ISO timestamp"
    assert data.get("deferred_reason") == "stale spec"
    assert data.get("status") == "deferred"
    assert not data.get("completed_at"), "completed_at must remain clear when deferring"


# --- reprioritize ---

def test_ac1_hp_graph_reprioritize(tmp_graph):
    """AC1-HP: fno graph reprioritize changes priority."""
    r = _invoke("graph", "add", "ToRepri")
    node_id = json.loads(r.output)["id"]

    r = _invoke("graph", "reprioritize", node_id, "p1")
    assert r.exit_code == 0

    r = _invoke("graph", "get", node_id)
    data = json.loads(r.output)
    assert data["priority"] == "p1"


# --- rank (ab-95a4a479: curated intra-lane ordering) ---

def _add(title: str, *, project: str, priority: str) -> str:
    r = _invoke("backlog", "add", title, "--project", project, "--priority", priority)
    assert r.exit_code == 0, r.output
    return json.loads(r.output)["id"]


def _rank_of(g: Path, node_id: str):
    for e in _read_graph(g):
        if e["id"] == node_id:
            return e.get("rank")
    raise AssertionError(f"{node_id} not in graph")


def test_ac1_hp_rank_top_pins_to_lane_front(tmp_graph):
    """AC1-HP: `rank A --top` sorts A before B in the same lane on the board."""
    a = _add("AlphaCard", project="fno", priority="p1")  # Now/abilities
    b = _add("BetaCard", project="fno", priority="p1")   # Now/abilities

    r = _invoke("backlog", "rank", a, "--top")
    assert r.exit_code == 0, r.output
    assert "--top" in r.output and a in r.output

    # A is now ranked, B remains unranked.
    assert _rank_of(tmp_graph, a) is not None
    assert _rank_of(tmp_graph, b) is None

    # And on the rendered board, A leads B within the Now column.
    md = (tmp_graph.parent / "graph.md").read_text()
    now_body = md.split("## Now", 1)[1].split("\n## ", 1)[0]
    assert now_body.index("AlphaCard") < now_body.index("BetaCard")


def test_ac1_ui_ranked_card_leads_lane_after_before(tmp_graph):
    """AC1-UI: --before a ranked anchor places the card ahead of it on the board."""
    a = _add("FirstCard", project="fno", priority="p1")
    b = _add("SecondCard", project="fno", priority="p1")
    assert _invoke("backlog", "rank", a, "--top").exit_code == 0
    r = _invoke("backlog", "rank", b, "--before", a)
    assert r.exit_code == 0, r.output
    assert _rank_of(tmp_graph, b) < _rank_of(tmp_graph, a)
    md = (tmp_graph.parent / "graph.md").read_text()
    now_body = md.split("## Now", 1)[1].split("\n## ", 1)[0]
    assert now_body.index("SecondCard") < now_body.index("FirstCard")


def test_ac1_after_ranked_anchor_places_behind(tmp_graph):
    """--after a ranked anchor places the card behind it (own midpoint branch)."""
    a = _add("LeadCard", project="fno", priority="p1")
    b = _add("TrailCard", project="fno", priority="p1")
    assert _invoke("backlog", "rank", a, "--top").exit_code == 0
    r = _invoke("backlog", "rank", b, "--after", a)
    assert r.exit_code == 0, r.output
    assert _rank_of(tmp_graph, b) > _rank_of(tmp_graph, a)
    md = (tmp_graph.parent / "graph.md").read_text()
    now_body = md.split("## Now", 1)[1].split("\n## ", 1)[0]
    assert now_body.index("LeadCard") < now_body.index("TrailCard")


def test_rank_self_anchor_rejected(tmp_graph):
    """A node cannot be ranked relative to itself (Failure Mode: self-anchor)."""
    a = _add("Solo", project="fno", priority="p1")
    assert _invoke("backlog", "rank", a, "--top").exit_code == 0
    r = _invoke("backlog", "rank", a, "--before", a)
    assert r.exit_code != 0
    assert "itself" in r.output


def test_rank_partial_id_resolves_and_guards_self(tmp_graph):
    """A partial id fuzzy-resolves; the resolved id (not the raw partial) is
    used for self-exclusion and the self-anchor guard."""
    a = _add("PartialCard", project="fno", priority="p1")
    partial = a[:7]  # 'ab-' + 4 hex, unique with a single node
    # Partial resolves and ranks the full node.
    r = _invoke("backlog", "rank", partial, "--top")
    assert r.exit_code == 0, r.output
    assert _rank_of(tmp_graph, a) is not None
    # Partial self-anchor is still caught (resolved id == resolved anchor id).
    r2 = _invoke("backlog", "rank", partial, "--after", partial)
    assert r2.exit_code != 0
    assert "itself" in r2.output


def test_rank_top_ignores_nonfinite_peer(tmp_graph):
    """A peer with a non-finite rank (hand-edited graph.json) is treated as
    unranked, so --top computes a finite rank instead of NaN/inf and the
    command succeeds (review: gemini medium + codex P2)."""
    # Seed two same-lane nodes directly: one poisoned (rank=inf), one clean.
    tmp_graph.write_text(json.dumps({"entries": [
        {"id": "ab-poison01", "title": "Poison", "project": "fno",
         "priority": "p1", "rank": float("inf"), "created_at": "2026-01-01T00:00:00Z"},
        {"id": "ab-clean001", "title": "Clean", "project": "fno",
         "priority": "p1", "rank": None, "created_at": "2026-01-02T00:00:00Z"},
    ]}) + "\n")
    r = _invoke("backlog", "rank", "ab-clean001", "--top")
    assert r.exit_code == 0, r.output
    new_rank = _rank_of(tmp_graph, "ab-clean001")
    assert new_rank is not None
    assert new_rank == new_rank  # finite: NaN != NaN would fail this
    assert new_rank not in (float("inf"), float("-inf"))


def test_ac1_err_cross_lane_anchor_rejected(tmp_graph):
    """AC1-ERR: --before across lanes errors naming both lanes, exits non-zero,
    and writes no rank to the target."""
    a = _add("WebCard", project="web", priority="p1")   # Now/web
    b = _add("EtlCard", project="etl", priority="p1")   # Now/etl

    r = _invoke("backlog", "rank", a, "--before", b)
    assert r.exit_code != 0
    assert "Now/web" in r.output and "Now/etl" in r.output
    # No rank written to A.
    assert _rank_of(tmp_graph, a) is None


def test_ac1_edge_only_node_in_lane_bottom(tmp_graph):
    """AC1-EDGE: --bottom on the sole node in a lane succeeds with a valid rank."""
    a = _add("LonelyCard", project="fno", priority="p3")  # Later/abilities (alone)
    r = _invoke("backlog", "rank", a, "--bottom")
    assert r.exit_code == 0, r.output
    assert isinstance(_rank_of(tmp_graph, a), (int, float))


def test_rank_clear_resets_to_unranked(tmp_graph):
    """--clear returns a ranked node to the unranked flow (rank=null)."""
    a = _add("ClearMe", project="fno", priority="p1")
    assert _invoke("backlog", "rank", a, "--top").exit_code == 0
    assert _rank_of(tmp_graph, a) is not None
    r = _invoke("backlog", "rank", a, "--clear")
    assert r.exit_code == 0, r.output
    assert _rank_of(tmp_graph, a) is None


def test_rank_requires_exactly_one_flag(tmp_graph):
    a = _add("NoFlag", project="fno", priority="p1")
    assert _invoke("backlog", "rank", a).exit_code != 0          # zero flags
    assert _invoke("backlog", "rank", a, "--top", "--bottom").exit_code != 0  # two flags


def test_rank_nonexistent_node_errors(tmp_graph):
    r = _invoke("backlog", "rank", "ab-deadbeef", "--top")
    assert r.exit_code != 0
    assert "not found" in r.output


def test_rank_unranked_anchor_rejected(tmp_graph):
    """--before an unranked anchor errors with an actionable hint (band model:
    you position relative to other ranked cards)."""
    a = _add("AnchorMe", project="fno", priority="p1")
    b = _add("MoveMe", project="fno", priority="p1")
    r = _invoke("backlog", "rank", b, "--before", a)  # a is unranked
    assert r.exit_code != 0
    assert "unranked" in r.output
    assert _rank_of(tmp_graph, b) is None


# --- archive ---

def test_ac1_hp_graph_archive(tmp_graph):
    """AC1-HP: fno graph archive moves done nodes (#23).

    Replaces the prior exit-code-only assertion with a state round-trip
    through the archive file. A weak exit-code check could mask a
    regression where archive prints success but does not actually move
    the node off the live graph or into the archive file.

    The sweep is now dry-run by default with a 30-day age filter, so a
    freshly-completed node needs `--apply --older-than-days 0` to move.
    """
    r = _invoke("graph", "add", "ToArchive")
    node_id = json.loads(r.output)["id"]
    # Seed completed_at in the fixture rather than via a CLI verb: closing is
    # merge-gated now, and archive only cares that the node reads done.
    graph = json.loads(tmp_graph.read_text())
    for entry in graph["entries"]:
        if entry["id"] == node_id:
            entry["completed_at"] = "2026-01-01T00:00:00+00:00"
    tmp_graph.write_text(json.dumps(graph))

    # Dry-run default: nothing moves.
    r = _invoke("graph", "archive")
    assert r.exit_code == 0
    assert "dry-run" in r.output
    assert not (tmp_graph.parent / "graph-archive.json").exists()

    r = _invoke("graph", "archive", "--apply", "--older-than-days", "0")
    assert r.exit_code == 0

    # State round-trip (#23): the node must be GONE from graph.json AND
    # PRESENT in graph-archive.json. Both directions matter; checking
    # only one half would miss "moved but not removed" or "removed but
    # not archived" regressions.
    archive_path = tmp_graph.parent / "graph-archive.json"
    assert archive_path.exists(), "archive file should exist after archive"
    archive_entries = json.loads(archive_path.read_text())["entries"]
    archived_ids = {e["id"] for e in archive_entries}
    assert node_id in archived_ids, f"{node_id} should be in archive"

    live_entries = _read_graph(tmp_graph)
    live_ids = {e["id"] for e in live_entries}
    assert node_id not in live_ids, f"{node_id} should be removed from live graph"


# --- priority vocabulary migration (p0/p1/p2/p3) ---

def test_priority_p0_accepted(tmp_graph):
    """`backlog add "X" --priority p0` succeeds; node has priority="p0"."""
    r = _invoke("backlog", "add", "Drop everything", "--priority", "p0")
    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    assert entries[0]["priority"] == "p0"


def test_priority_default_is_p2(tmp_graph):
    """`backlog add "X"` without --priority creates a node with priority="p2"."""
    r = _invoke("backlog", "add", "Default priority")
    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    assert entries[0]["priority"] == "p2"


def test_priority_migration_on_mutation(tmp_graph):
    """Legacy high/medium/low values are backfilled to p1/p2/p3 on the
    next graph mutation (recompute_statuses runs inside locked_mutate_graph).
    """
    tmp_graph.write_text(json.dumps({
        "entries": [
            {"id": "ab-old00001", "title": "Was high", "priority": "high",
             "plan_path": "x.md", "status": "ready",
             "created_at": "2026-01-01T00:00:00Z"},
            {"id": "ab-old00002", "title": "Was medium", "priority": "medium",
             "plan_path": "x.md", "status": "ready",
             "created_at": "2026-01-01T00:00:00Z"},
            {"id": "ab-old00003", "title": "Was low", "priority": "low",
             "plan_path": "x.md", "status": "ready",
             "created_at": "2026-01-01T00:00:00Z"},
        ]
    }))

    # Trigger a mutation; locked_mutate_graph runs recompute_statuses
    # which contains the backfill loop.
    r = _invoke("backlog", "add", "Trigger mutation")
    assert r.exit_code == 0, r.output

    entries = _read_graph(tmp_graph)
    by_id = {e["id"]: e for e in entries}
    assert by_id["ab-old00001"]["priority"] == "p1"
    assert by_id["ab-old00002"]["priority"] == "p2"
    assert by_id["ab-old00003"]["priority"] == "p3"
    # No old vocabulary survives.
    assert all(
        e["priority"] in {"p0", "p1", "p2", "p3"}
        for e in entries
    )


def test_priority_old_vocabulary_rejected(tmp_graph):
    """`backlog add "X" --priority high` exits non-zero with an error
    message that lists the new p0|p1|p2|p3 vocabulary.
    """
    r = runner.invoke(
        app, ["backlog", "add", "Old syntax", "--priority", "high"],
        catch_exceptions=True,
    )
    assert r.exit_code != 0
    # Error goes to stderr; CliRunner combines streams unless mix_stderr=False.
    combined = (r.output or "") + (getattr(r, "stderr", "") or "")
    assert "p0" in combined and "p1" in combined and "p2" in combined and "p3" in combined


def test_priority_order_sort(tmp_graph):
    """`PRIORITY_ORDER` ranks p0 < p1 < p2 < p3 (lower index = higher priority)."""
    from fno.graph._constants import PRIORITY_ORDER
    assert PRIORITY_ORDER["p0"] < PRIORITY_ORDER["p1"]
    assert PRIORITY_ORDER["p1"] < PRIORITY_ORDER["p2"]
    assert PRIORITY_ORDER["p2"] < PRIORITY_ORDER["p3"]


def test_priority_migration_idempotent(tmp_graph):
    """Running the backfill twice is a no-op (the plan's claim)."""
    tmp_graph.write_text(json.dumps({
        "entries": [
            {"id": "ab-old00001", "title": "Was high", "priority": "high",
             "plan_path": "x.md", "status": "ready",
             "created_at": "2026-01-01T00:00:00Z"},
        ]
    }))
    # First mutation: backfill runs.
    _invoke("backlog", "add", "Trigger 1")
    after_first = json.loads(tmp_graph.read_text())
    # Second mutation: the row is already on the new vocabulary; no thrash.
    _invoke("backlog", "add", "Trigger 2")
    after_second = json.loads(tmp_graph.read_text())

    legacy_after_first = next(e for e in after_first["entries"] if e["id"] == "ab-old00001")
    legacy_after_second = next(e for e in after_second["entries"] if e["id"] == "ab-old00001")
    assert legacy_after_first["priority"] == "p1"
    assert legacy_after_second["priority"] == "p1"


def test_priority_missing_key_backfill(tmp_graph):
    """An entry with no priority key gets the default p2 via _apply_graph_defaults
    rather than being touched by the backfill loop (which only rewrites legacy
    string values).
    """
    tmp_graph.write_text(json.dumps({
        "entries": [
            {"id": "ab-nokey0001", "title": "No priority key",
             "plan_path": "x.md", "status": "ready",
             "created_at": "2026-01-01T00:00:00Z"},
        ]
    }))
    _invoke("backlog", "add", "Trigger mutation")
    entries = _read_graph(tmp_graph)
    nokey = next(e for e in entries if e["id"] == "ab-nokey0001")
    assert nokey["priority"] == "p2"


def test_priority_update_rejects_invalid(tmp_graph):
    """`backlog update <id> --priority garbage` exits non-zero (closes the
    silent graph-corruption gap surfaced during sigma-review).
    """
    r = _invoke("backlog", "add", "Updateme")
    node_id = json.loads(r.output)["id"]

    bad = runner.invoke(
        app,
        ["backlog", "update", node_id, "--priority", "garbage"],
        catch_exceptions=True,
    )
    assert bad.exit_code != 0
    combined = (bad.output or "") + (getattr(bad, "stderr", "") or "")
    assert "p0" in combined and "p1" in combined and "p2" in combined and "p3" in combined


def test_model_pin_set_visible_and_cleared(tmp_graph):
    """x-571f US2 AC1-UI: `backlog update <id> --model` sets the pin (visible in
    `get`), and `--model null` clears it back to provider default."""
    node_id = json.loads(_invoke("backlog", "add", "Pinme").output)["id"]

    r = _invoke("backlog", "update", node_id, "--model", "fable")
    assert r.exit_code == 0, r.output
    got = json.loads(_invoke("backlog", "get", node_id).output)
    assert got["model"] == "fable"

    # 'null' clears (revert to default).
    r = _invoke("backlog", "update", node_id, "--model", "null")
    assert r.exit_code == 0, r.output
    got = json.loads(_invoke("backlog", "get", node_id).output)
    assert got["model"] is None


def test_model_pin_rejects_whitespace_token(tmp_graph):
    """x-571f US2 AC1-ERR: a whitespaced OR glob/shell-metacharacter model exits
    non-zero and leaves the node unchanged (protects the unquoted MODEL_FLAG /
    dispatch-node.sh spawn against word-splitting AND globbing; gemini review)."""
    node_id = json.loads(_invoke("backlog", "add", "Nopin").output)["id"]

    for bad_val in ("opus 4.8", "fo*", "a?b", "x[y]"):
        bad = runner.invoke(
            app,
            ["backlog", "update", node_id, "--model", bad_val],
            catch_exceptions=True,
        )
        assert bad.exit_code != 0, f"{bad_val!r} should be rejected"
        combined = (bad.output or "") + (getattr(bad, "stderr", "") or "")
        assert "single token" in combined
        # Node unchanged: model still unset after every rejection.
        got = json.loads(_invoke("backlog", "get", node_id).output)
        assert got.get("model") is None

    # A full provider-model id with dots/slashes/dashes/colons is accepted.
    ok = _invoke("backlog", "update", node_id, "--model", "openai/gpt-4.1")
    assert ok.exit_code == 0, ok.output
    assert json.loads(_invoke("backlog", "get", node_id).output)["model"] == "openai/gpt-4.1"


def test_model_pin_rides_in_ready_and_next_json(tmp_graph):
    """x-571f US3: the model pin must ride in the `ready` and `next` JSON so the
    lane-fill (`_ready_nodes`) and sequential-drain (`fno backlog next`)
    dispatchers can thread it into the spawn they build (AC1-HP / AC2-HP)."""
    tmp_graph.write_text(json.dumps({
        "entries": [
            {"id": "ab-pinned00", "title": "Pinned", "priority": "p1",
             "plan_path": "x.md", "status": "ready", "model": "fable",
             "created_at": _recent_iso(1)},
        ]
    }))

    listing = json.loads(_invoke("backlog", "ready", "--all").stdout)
    assert listing[0]["model"] == "fable"

    nxt = json.loads(_invoke("backlog", "next", "--all").stdout)
    assert nxt["model"] == "fable"


def test_priority_read_path_backfill(tmp_graph):
    """Read-only commands (`backlog ready`/`next`) sort correctly even before
    the first mutation triggers the on-disk backfill - `_apply_graph_defaults`
    rewrites legacy values in memory.
    """
    tmp_graph.write_text(json.dumps({
        "entries": [
            {"id": "ab-mem00low", "title": "Was low", "priority": "low",
             "plan_path": "x.md", "status": "ready",
             "created_at": _recent_iso(2)},
            {"id": "ab-mem00hi0", "title": "Was high", "priority": "high",
             "plan_path": "x.md", "status": "ready",
             "created_at": _recent_iso(1)},
        ]
    }))

    r = _invoke("backlog", "ready", "--all")
    assert r.exit_code == 0, r.output
    listing = json.loads(r.stdout)
    # Was-high (p1, rank 1) must come before was-low (p3, rank 3).
    titles = [e["title"] for e in listing]
    assert titles.index("Was high") < titles.index("Was low")
    # And the in-memory rows reflect the migrated vocabulary.
    priorities = {e["title"]: e["priority"] for e in listing}
    assert priorities["Was high"] == "p1"
    assert priorities["Was low"] == "p3"


# --- additional_prs (--add-pr / --remove-pr) ---

def test_add_pr_appends_to_additional_prs(tmp_graph):
    """--add-pr 542 appends an entry to additional_prs."""
    r = _invoke("backlog", "add", "Multi")
    node_id = json.loads(r.output)["id"]

    r = _invoke(
        "backlog", "update", node_id,
        "--add-pr", "542",
        "--add-pr-url", "https://github.com/x/y/pull/542",
        "--add-pr-note", "wrap-up",
    )
    assert r.exit_code == 0, r.output

    node = json.loads(_invoke("backlog", "get", node_id).output)
    assert node["additional_prs"] == [
        {"number": 542, "url": "https://github.com/x/y/pull/542", "note": "wrap-up"},
    ]


def test_add_pr_dedups_on_same_number(tmp_graph):
    """Re-adding the same PR number updates that entry in place, not duplicates."""
    r = _invoke("backlog", "add", "Multi")
    node_id = json.loads(r.output)["id"]

    _invoke("backlog", "update", node_id, "--add-pr", "542", "--add-pr-note", "first")
    _invoke("backlog", "update", node_id, "--add-pr", "542", "--add-pr-note", "updated")

    node = json.loads(_invoke("backlog", "get", node_id).output)
    assert len(node["additional_prs"]) == 1
    assert node["additional_prs"][0]["number"] == 542
    assert node["additional_prs"][0]["note"] == "updated"


def test_add_pr_minimal_derives_the_url(tmp_graph, monkeypatch):
    """--add-pr alone is valid, but never url-less: additional_prs entries are
    read by the same repo-scoped matcher, so a bare number is unattributable."""
    import fno.graph._reconcile as rec

    monkeypatch.setattr(
        rec, "pr_url_for_repo", lambda pr, cwd=None: f"https://github.com/o/r/pull/{pr}"
    )
    r = _invoke("backlog", "add", "Numbered")
    node_id = json.loads(r.output)["id"]

    r = _invoke("backlog", "update", node_id, "--add-pr", "777")
    assert r.exit_code == 0, r.output

    node = json.loads(_invoke("backlog", "get", node_id).output)
    assert node["additional_prs"] == [
        {"number": 777, "url": "https://github.com/o/r/pull/777"}
    ]


def test_remove_pr_drops_entry_by_number(tmp_graph):
    """--remove-pr N drops the entry with that number from additional_prs."""
    r = _invoke("backlog", "add", "Multi")
    node_id = json.loads(r.output)["id"]
    _invoke("backlog", "update", node_id, "--add-pr", "542")
    _invoke("backlog", "update", node_id, "--add-pr", "543")

    r = _invoke("backlog", "update", node_id, "--remove-pr", "542")
    assert r.exit_code == 0, r.output

    node = json.loads(_invoke("backlog", "get", node_id).output)
    numbers = [e["number"] for e in node["additional_prs"]]
    assert numbers == [543]


def test_remove_pr_missing_is_noop(tmp_graph):
    """--remove-pr on an absent number is a no-op (no error)."""
    r = _invoke("backlog", "add", "Multi")
    node_id = json.loads(r.output)["id"]
    _invoke("backlog", "update", node_id, "--add-pr", "542")

    r = _invoke("backlog", "update", node_id, "--remove-pr", "999")
    assert r.exit_code == 0, r.output

    node = json.loads(_invoke("backlog", "get", node_id).output)
    assert [e["number"] for e in node["additional_prs"]] == [542]


def test_add_pr_metadata_without_add_pr_errors(tmp_graph):
    """--add-pr-url or --add-pr-note without --add-pr exits non-zero.

    Today they would be silently ignored; Gemini flagged this as a UX
    pitfall on PR #316. Fail loudly so users notice the missing --add-pr.
    """
    r = _invoke("backlog", "add", "Strict")
    node_id = json.loads(r.output)["id"]

    r = runner.invoke(
        app, ["backlog", "update", node_id, "--add-pr-url", "https://x/y/pull/1"],
        catch_exceptions=True,
    )
    assert r.exit_code != 0
    combined = (r.output or "") + (getattr(r, "stderr", "") or "")
    assert "--add-pr" in combined and "require" in combined.lower()

    r = runner.invoke(
        app, ["backlog", "update", node_id, "--add-pr-note", "stray note"],
        catch_exceptions=True,
    )
    assert r.exit_code != 0
    combined = (r.output or "") + (getattr(r, "stderr", "") or "")
    assert "--add-pr" in combined and "require" in combined.lower()


def test_legacy_entry_without_additional_prs_loads_with_default(tmp_graph):
    """Old graph.json entries (no additional_prs key) get [] on read."""
    tmp_graph.write_text(json.dumps({"entries": [
        {"id": "ab-12345678", "title": "Legacy", "priority": "p2",
         "type": "feature", "domain": "code", "parent": None,
         "plan_path": "x.md", "completed_at": "2026-01-01T00:00:00Z",
         "pr_number": 540, "pr_url": "https://github.com/x/y/pull/540",
         "created_at": "2026-01-01T00:00:00Z"}
    ]}))
    r = _invoke("backlog", "get", "ab-12345678")
    assert r.exit_code == 0, r.output
    data = json.loads(r.output)
    assert data.get("additional_prs") == []


def test_render_html_renders_non_http_additional_pr_url_as_plain_text(tmp_path):
    """REGRESSION (Codex P2 on PR #316): when an additional_prs entry's
    url is present but not http(s) (e.g. 'github.com/x/y/pull/542' without
    scheme), the HTML renderer was silently dropping the url and emitting
    only '#<number>'. Mirror the primary pr_url fallback so the URL stays
    visible as escaped plain text -- consistent with the markdown renderer
    and the primary-PR HTML path.
    """
    from fno.graph.render_html import _card_html

    entry = {
        "id": "ab-abcdabcd", "title": "Multi", "priority": "p2",
        "type": "feature", "domain": "code", "parent": None,
        "plan_path": "x.md", "completed_at": "2026-01-01T00:00:00Z",
        "pr_number": 540, "pr_url": "https://github.com/x/y/pull/540",
        "additional_prs": [
            {"number": 542, "url": "github.com/x/y/pull/542", "note": "no-scheme"},
        ],
        "created_at": "2026-01-01T00:00:00Z",
        "status": "done",
    }
    html_out = _card_html(entry, {entry["id"]: entry})
    # The URL must be visible somewhere even though it has no scheme.
    assert "github.com/x/y/pull/542" in html_out, (
        f"non-http url silently dropped: {html_out!r}"
    )
    # Must NOT be linkified as an anchor (would be a clickability footgun).
    assert '<a href="github.com/x/y/pull/542"' not in html_out
    # The note still surfaces.
    assert "no-scheme" in html_out


def test_render_md_includes_additional_prs_on_done_nodes(tmp_graph):
    """Tree rendering surfaces additional_prs URLs for done nodes."""
    tmp_graph.write_text(json.dumps({"entries": [
        {"id": "ab-87878787", "title": "Multi", "priority": "p2",
         "type": "feature", "domain": "code", "parent": None,
         "plan_path": "x.md", "completed_at": "2026-01-01T00:00:00Z",
         "pr_number": 540, "pr_url": "https://github.com/x/y/pull/540",
         "additional_prs": [
             {"number": 542, "url": "https://github.com/x/y/pull/542", "note": "wrap-up"},
             {"number": 543, "url": "https://github.com/x/y/pull/543"},
         ],
         "created_at": "2026-01-01T00:00:00Z"}
    ]}))
    from fno.graph.store import read_graph
    from fno.graph.render import render_graph_md
    entries = read_graph(tmp_graph)
    md_path = tmp_graph.parent / "graph.md"
    render_graph_md(entries, md_path)
    text = md_path.read_text()
    assert "https://github.com/x/y/pull/540" in text
    assert "https://github.com/x/y/pull/542" in text
    assert "wrap-up" in text
    assert "https://github.com/x/y/pull/543" in text


# --- --completion-note setter ---

def test_update_completion_note_sets_on_empty(tmp_graph):
    """--completion-note sets the value when no existing note."""
    r = _invoke("backlog", "add", "Target")
    node_id = json.loads(r.output)["id"]

    r = _invoke("backlog", "update", node_id, "--completion-note", "PR #543 shipped")
    assert r.exit_code == 0, r.output

    node = json.loads(_invoke("backlog", "get", node_id).output)
    assert node["completion_note"] == "PR #543 shipped"


def test_update_completion_note_appends_to_existing(tmp_graph):
    """Two --completion-note calls append, not replace; separator is ' + '."""
    r = _invoke("backlog", "add", "MultiPR")
    node_id = json.loads(r.output)["id"]

    _invoke("backlog", "update", node_id, "--completion-note", "PR #542 (wrap-up)")
    _invoke("backlog", "update", node_id, "--completion-note", "PR #543 (followups)")

    node = json.loads(_invoke("backlog", "get", node_id).output)
    assert node["completion_note"] == "PR #542 (wrap-up) + PR #543 (followups)"


def test_update_completion_note_whitespace_only_is_noop(tmp_graph):
    """--completion-note with whitespace-only value leaves the field unchanged."""
    r = _invoke("backlog", "add", "Whitespace")
    node_id = json.loads(r.output)["id"]
    _invoke("backlog", "update", node_id, "--completion-note", "Real note")

    r = _invoke("backlog", "update", node_id, "--completion-note", "   ")
    assert r.exit_code == 0, r.output

    node = json.loads(_invoke("backlog", "get", node_id).output)
    assert node["completion_note"] == "Real note"


def test_update_completion_note_null_clears(tmp_graph):
    """--completion-note null clears the field, mirroring --parent null."""
    r = _invoke("backlog", "add", "Target")
    node_id = json.loads(r.output)["id"]
    _invoke("backlog", "update", node_id, "--completion-note", "set before clearing")

    r = _invoke("backlog", "update", node_id, "--completion-note", "null")
    assert r.exit_code == 0, r.output

    node = json.loads(_invoke("backlog", "get", node_id).output)
    assert node["completion_note"] is None


def test_update_completion_note_unknown_node_errors(tmp_graph):
    """--completion-note on a missing node exits non-zero."""
    r = runner.invoke(
        app, ["backlog", "update", "ab-deadbeef", "--completion-note", "x"],
        catch_exceptions=True,
    )
    assert r.exit_code != 0


# --- --parent setter ---

def _add_with_parent_chain(g: Path) -> tuple[str, str, str]:
    """Helper: create three nodes a -> b -> c, returning their IDs.

    Chain is built via direct graph.json writes since `backlog add` doesn't
    accept --parent (the gap being fixed is post-hoc parent edit, not
    intake-time parent). Returns (a_id, b_id, c_id) where c.parent == b,
    b.parent == a, a.parent == None.
    """
    entries = [
        {"id": "ab-aaaaaaaa", "title": "A", "priority": "p2", "parent": None,
         "domain": "code", "type": "feature",
         "created_at": "2026-01-01T00:00:00Z"},
        {"id": "ab-bbbbbbbb", "title": "B", "priority": "p2", "parent": "ab-aaaaaaaa",
         "domain": "code", "type": "feature",
         "created_at": "2026-01-02T00:00:00Z"},
        {"id": "ab-cccccccc", "title": "C", "priority": "p2", "parent": "ab-bbbbbbbb",
         "domain": "code", "type": "feature",
         "created_at": "2026-01-03T00:00:00Z"},
    ]
    g.write_text(json.dumps({"entries": entries}))
    return "ab-aaaaaaaa", "ab-bbbbbbbb", "ab-cccccccc"


def test_update_parent_sets_value_on_orphan(tmp_graph):
    """--parent sets a previously-null parent."""
    r = _invoke("backlog", "add", "Orphan")
    orphan_id = json.loads(r.output)["id"]
    a_id, _, _ = _add_with_parent_chain(tmp_graph)
    # _add_with_parent_chain overwrote orphan; re-add it preserving the chain.
    entries = _read_graph(tmp_graph)
    entries.append({"id": orphan_id, "title": "Orphan", "priority": "p2",
                    "parent": None, "domain": "code", "type": "feature",
                    "created_at": "2026-01-04T00:00:00Z"})
    tmp_graph.write_text(json.dumps({"entries": entries}))

    r = _invoke("backlog", "update", orphan_id, "--parent", a_id)
    assert r.exit_code == 0, r.output

    node = json.loads(_invoke("backlog", "get", orphan_id).output)
    assert node["parent"] == a_id


def test_update_parent_null_clears(tmp_graph):
    """--parent null clears the parent (de-orphans to top-level)."""
    _, _, c_id = _add_with_parent_chain(tmp_graph)
    r = _invoke("backlog", "update", c_id, "--parent", "null")
    assert r.exit_code == 0, r.output

    node = json.loads(_invoke("backlog", "get", c_id).output)
    assert node["parent"] is None


def test_update_parent_unknown_target_errors(tmp_graph):
    """--parent <missing-id> exits non-zero; node's parent is unchanged."""
    _, _, c_id = _add_with_parent_chain(tmp_graph)
    original = json.loads(_invoke("backlog", "get", c_id).output)["parent"]

    r = runner.invoke(
        app, ["backlog", "update", c_id, "--parent", "ab-deadbeef"],
        catch_exceptions=True,
    )
    assert r.exit_code != 0
    combined = (r.output or "") + (getattr(r, "stderr", "") or "")
    assert "ab-deadbeef" in combined and "not found" in combined.lower()

    after = json.loads(_invoke("backlog", "get", c_id).output)["parent"]
    assert after == original


def test_update_parent_rejects_cycle_self(tmp_graph):
    """--parent <self-id> exits non-zero; node's parent is unchanged."""
    a_id, _, _ = _add_with_parent_chain(tmp_graph)
    original = json.loads(_invoke("backlog", "get", a_id).output)["parent"]

    r = runner.invoke(
        app, ["backlog", "update", a_id, "--parent", a_id],
        catch_exceptions=True,
    )
    assert r.exit_code != 0
    combined = (r.output or "") + (getattr(r, "stderr", "") or "")
    assert "cycle" in combined.lower()

    after = json.loads(_invoke("backlog", "get", a_id).output)["parent"]
    assert after == original


def test_update_parent_rejects_cycle_when_node_passed_as_fuzzy_prefix(tmp_graph):
    """REGRESSION (Codex P1 on PR #316): cycle check must use the resolved
    node id, not the raw CLI input. Before fix, passing the node id as a
    fuzzy prefix (e.g. 'ab-a' against 'ab-aaaaaaaa') would leave the seen
    set comparing the prefix against full ids in the ancestor walk -- the
    cycle would not trip and a parent loop would land on disk.
    """
    a_id, _, c_id = _add_with_parent_chain(tmp_graph)
    assert a_id == "ab-aaaaaaaa"  # sanity: fixture id is full-form
    assert c_id == "ab-cccccccc"

    # Fuzzy resolver requires 4-7 hex chars after "ab-". 4 is the minimum.
    r = runner.invoke(
        app, ["backlog", "update", "ab-aaaa", "--parent", "ab-cccc"],
        catch_exceptions=True,
    )
    assert r.exit_code != 0, (
        f"prefix-resolved parent re-assignment must trip the cycle check; "
        f"exit was {r.exit_code} output={r.output!r}"
    )
    combined = (r.output or "") + (getattr(r, "stderr", "") or "")
    assert "cycle" in combined.lower()
    # The on-disk parent must remain None (no silent corruption).
    after = json.loads(_invoke("backlog", "get", a_id).output)["parent"]
    assert after is None


def test_update_parent_rejects_cycle_via_descendant(tmp_graph):
    """--parent <descendant-id> exits non-zero (would create a cycle).

    Chain: a -> b -> c. Re-parenting a to c would form the cycle a <- c <- b <- a.
    """
    a_id, _, c_id = _add_with_parent_chain(tmp_graph)

    r = runner.invoke(
        app, ["backlog", "update", a_id, "--parent", c_id],
        catch_exceptions=True,
    )
    assert r.exit_code != 0
    combined = (r.output or "") + (getattr(r, "stderr", "") or "")
    assert "cycle" in combined.lower()


# --- unknown subcommand ---

def test_ac3_err_unknown_subcommand_exits_nonzero():
    """AC3-ERR: fno graph bogus exits non-zero."""
    r = runner.invoke(app, ["graph", "bogus"], catch_exceptions=True)
    assert r.exit_code != 0


# --- C3 (ab-82e65b72): epics-first selection precedence ---

def _epics_first_entries():
    """Epic (p2) with a p3 ready child, plus a p0 loose node.

    Epics-first must rank the p3 epic child ahead of the p0 loose node.
    """
    return [
        {"id": "ab-epic", "title": "Epic", "status": "ready", "priority": "p2",
         "created_at": _recent_iso(3), "project": "p", "blocked_by": [],
         "plan_path": "x.md"},
        {"id": "ab-child", "title": "Child", "status": "ready", "priority": "p3",
         "created_at": _recent_iso(2), "project": "p", "parent": "ab-epic",
         "blocked_by": [], "plan_path": "x.md"},
        {"id": "ab-loose", "title": "Loose", "status": "ready", "priority": "p0",
         "created_at": _recent_iso(1), "project": "p", "blocked_by": [],
         "plan_path": "x.md"},
    ]


def test_graph_next_picks_epic_child_over_higher_priority_loose(tmp_graph):
    """C3: `fno graph next` selects the epic child over a p0 loose node."""
    tmp_graph.write_text(json.dumps({"entries": _epics_first_entries()}) + "\n")
    r = _invoke("graph", "next", "--all")
    out = json.loads(r.stdout)
    assert out is not None
    assert out["id"] == "ab-child"


def test_graph_ready_orders_epic_children_before_loose(tmp_graph):
    """C3: `fno graph ready` lists epic children ahead of loose nodes."""
    tmp_graph.write_text(json.dumps({"entries": _epics_first_entries()}) + "\n")
    r = _invoke("graph", "ready", "--all")
    ids = [e["id"] for e in json.loads(r.stdout)]
    assert ids.index("ab-child") < ids.index("ab-loose")


def test_graph_ready_excludes_epics(tmp_graph):
    """x-33b2 (codex P2 on PR #69): `fno backlog ready` - which the
    `dispatch-node.sh --all-ready` bulk path enumerates - must NOT list a
    container, or that path would launch a /target worker against the box.
    Shares the epic filter with `next` so the two surfaces agree."""
    tmp_graph.write_text(json.dumps({"entries": _epics_first_entries()}) + "\n")
    r = _invoke("graph", "ready", "--all")
    ids = [e["id"] for e in json.loads(r.stdout)]
    assert "ab-epic" not in ids        # the container is excluded
    assert "ab-child" in ids           # its buildable leaf is listed
    assert "ab-loose" in ids


def test_graph_next_skips_in_progress_epic_for_leaf(tmp_graph):
    """x-33b2: an IN-PROGRESS epic (one child done, one still pending) is the
    top-ranked ready node but must NOT be selected - `next` falls through to a
    buildable leaf instead of repeatedly returning the container ('it keeps
    assuming this one is next')."""
    entries = [
        # Epic: ready, p0 -> would rank ahead of everything if selectable.
        {"id": "ab-epic", "title": "Epic", "status": "ready", "priority": "p0",
         "created_at": "2026-01-01", "project": "p", "blocked_by": [], "plan_path": "x.md"},
        # One child done, one still pending -> the epic is IN PROGRESS.
        {"id": "ab-cdone", "title": "Done child", "status": "done", "priority": "p2",
         "created_at": "2026-01-02", "project": "p", "parent": "ab-epic",
         "completed_at": "2026-01-03", "blocked_by": [], "plan_path": "x.md"},
        {"id": "ab-cpend", "title": "Pending child", "status": "ready", "priority": "p3",
         "created_at": _recent_iso(1), "project": "p", "parent": "ab-epic",
         "blocked_by": [], "plan_path": "x.md"},
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")
    r = _invoke("graph", "next", "--all")
    out = json.loads(r.stdout)
    assert out is not None
    assert out["id"] != "ab-epic"      # the in-progress container is skipped
    assert out["id"] == "ab-cpend"     # its buildable pending leaf is picked


def _by_id(tmp_graph):
    return {e["id"]: e for e in json.loads(tmp_graph.read_text())["entries"]}


def test_done_cascade_closes_all_done_parent_epic(tmp_graph):
    """x-33b2: closing the LAST open child of an epic auto-closes the epic (it is
    a container with no PR of its own; it is done when its children are). Replaces
    the old 'walker closes the epic via next' path."""
    entries = [
        {"id": "ab-epic0000", "title": "Epic", "status": "ready", "project": "p",
         "blocked_by": [], "plan_path": "x.md"},
        {"id": "ab-cdone001", "title": "Done child", "status": "done", "project": "p",
         "parent": "ab-epic0000", "completed_at": "2026-01-01T00:00:00Z", "blocked_by": []},
        # Last open child, no PR refs -> `done` closes it with no gh cross-check.
        {"id": "ab-clast002", "title": "Last child", "status": "ready", "project": "p",
         "parent": "ab-epic0000", "blocked_by": []},
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")
    r = _invoke("graph", "done", "ab-clast002")
    assert r.exit_code == 0, r.stdout + r.stderr
    nodes = _by_id(tmp_graph)
    assert nodes["ab-clast002"]["completed_at"]               # child closed
    assert nodes["ab-epic0000"]["completed_at"]                # epic auto-closed
    assert "auto-closed" in (nodes["ab-epic0000"].get("completion_note") or "")


def test_done_does_not_close_epic_with_a_pending_child(tmp_graph):
    """The cascade only fires when ALL children are done: an epic with another
    still-open child stays open."""
    entries = [
        {"id": "ab-epic0000", "title": "Epic", "status": "ready", "project": "p",
         "blocked_by": [], "plan_path": "x.md"},
        {"id": "ab-cdone001", "title": "Child A", "status": "ready", "project": "p",
         "parent": "ab-epic0000", "blocked_by": []},
        {"id": "ab-cstill02", "title": "Child B (stays open)", "status": "ready",
         "project": "p", "parent": "ab-epic0000", "blocked_by": []},
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")
    r = _invoke("graph", "done", "ab-cdone001")
    assert r.exit_code == 0, r.stdout + r.stderr
    nodes = _by_id(tmp_graph)
    assert nodes["ab-cdone001"]["completed_at"]
    assert not nodes["ab-epic0000"].get("completed_at")        # epic stays open


def test_done_cascade_closes_grandparent_chain(tmp_graph):
    """Cascade walks up multi-level chains: leaf -> sub-epic -> epic all close
    when the leaf (the only open node in the chain) lands."""
    entries = [
        {"id": "ab-epic0000", "title": "Epic", "status": "ready", "project": "p",
         "blocked_by": [], "plan_path": "x.md"},
        {"id": "ab-sub00001", "title": "Sub-epic", "status": "ready", "project": "p",
         "parent": "ab-epic0000", "blocked_by": []},
        {"id": "ab-leaf0002", "title": "Leaf", "status": "ready", "project": "p",
         "parent": "ab-sub00001", "blocked_by": []},
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")
    r = _invoke("graph", "done", "ab-leaf0002")
    assert r.exit_code == 0, r.stdout + r.stderr
    nodes = _by_id(tmp_graph)
    assert nodes["ab-leaf0002"]["completed_at"]
    assert nodes["ab-sub00001"]["completed_at"]               # sub-epic closed
    assert nodes["ab-epic0000"]["completed_at"]                # grandparent closed


def test_done_cascade_closes_cross_project_parent(tmp_graph):
    """The cascade follows the parent EDGE, not a project filter: a parent in a
    DIFFERENT project from its child closes on the same close - the cross-project
    closure gap codex flagged (advance() is project-scoped and cannot)."""
    entries = [
        {"id": "ab-epic0000", "title": "Epic", "status": "ready", "project": "web",
         "blocked_by": [], "plan_path": "x.md"},
        {"id": "ab-leaf0001", "title": "Leaf", "status": "ready", "project": "etl",
         "parent": "ab-epic0000", "blocked_by": []},
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")
    r = _invoke("graph", "done", "ab-leaf0001")
    assert r.exit_code == 0, r.stdout + r.stderr
    nodes = _by_id(tmp_graph)
    assert nodes["ab-epic0000"]["completed_at"]                # closed despite diff project


# ---------------------------------------------------------------------------
# _resolved_cwd derivation in cmd_get
# ---------------------------------------------------------------------------

def _make_node_with_project_cwd(project: str, cwd: str) -> dict:
    return {
        "id": "ab-resolvetest",
        "title": "Resolve Test",
        "status": "ready",
        "project": project,
        "cwd": cwd,
    }


def test_resolved_cwd_uses_work_map_root_when_project_mapped(tmp_graph):
    """AC1: node with project mapped in settings -> _resolved_cwd == work-map root."""
    import textwrap
    from unittest.mock import patch

    node = _make_node_with_project_cwd("myproject", "/recorded/other")
    tmp_graph.write_text(json.dumps({"entries": [node]}) + "\n")

    # Write a tmp settings file mapping myproject -> /mapped/root
    settings_path = tmp_graph.parent / "settings.yaml"
    settings_path.write_text(textwrap.dedent("""\
        work:
          workspaces:
            main:
              projects:
                - name: myproject
                  path: /mapped/root
    """))

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    ):
        r = _invoke("graph", "get", "ab-resolvetest")

    assert r.exit_code == 0, r.output
    data = json.loads(r.output)
    assert data["_resolved_cwd"] == "/mapped/root", (
        f"Expected /mapped/root, got {data.get('_resolved_cwd')!r}"
    )


def test_resolved_cwd_falls_back_to_recorded_cwd_when_unmapped(tmp_graph):
    """AC2: node with unmapped project -> _resolved_cwd == recorded cwd."""
    import textwrap
    from unittest.mock import patch

    node = _make_node_with_project_cwd("unmapped-project", "/recorded/cwd")
    tmp_graph.write_text(json.dumps({"entries": [node]}) + "\n")

    settings_path = tmp_graph.parent / "settings.yaml"
    settings_path.write_text(textwrap.dedent("""\
        work:
          workspaces:
            main:
              projects:
                - name: other-project
                  path: /some/path
    """))

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    ):
        r = _invoke("graph", "get", "ab-resolvetest")

    assert r.exit_code == 0, r.output
    data = json.loads(r.output)
    assert data["_resolved_cwd"] == "/recorded/cwd"


def test_resolved_cwd_falls_back_to_recorded_cwd_when_project_null(tmp_graph):
    """AC3: node with no project -> _resolved_cwd == recorded cwd."""
    node = {
        "id": "ab-resolvetest",
        "title": "Null Project",
        "status": "ready",
        "project": None,
        "cwd": "/recorded/cwd",
    }
    tmp_graph.write_text(json.dumps({"entries": [node]}) + "\n")

    r = _invoke("graph", "get", "ab-resolvetest")
    assert r.exit_code == 0, r.output
    data = json.loads(r.output)
    assert data["_resolved_cwd"] == "/recorded/cwd"


def test_resolved_cwd_field_flag_works(tmp_graph):
    """AC4: --field _resolved_cwd prints the derived value."""
    import textwrap
    from unittest.mock import patch

    node = _make_node_with_project_cwd("myproject", "/recorded/other")
    tmp_graph.write_text(json.dumps({"entries": [node]}) + "\n")

    settings_path = tmp_graph.parent / "settings.yaml"
    settings_path.write_text(textwrap.dedent("""\
        work:
          projects:
            myproject:
              path: /mapped/root
    """))

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    ):
        r = _invoke("graph", "get", "ab-resolvetest", "--field", "_resolved_cwd")

    assert r.exit_code == 0, r.output
    assert r.output.strip() == "/mapped/root"


def test_resolved_cwd_never_persisted_to_graph_json(tmp_graph):
    """AC5: _resolved_cwd is never written back to the graph.json on disk."""
    import textwrap
    from unittest.mock import patch

    node = _make_node_with_project_cwd("myproject", "/recorded/other")
    tmp_graph.write_text(json.dumps({"entries": [node]}) + "\n")

    settings_path = tmp_graph.parent / "settings.yaml"
    settings_path.write_text(textwrap.dedent("""\
        work:
          projects:
            myproject:
              path: /mapped/root
    """))

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    ):
        _invoke("graph", "get", "ab-resolvetest")

    disk_data = json.loads(tmp_graph.read_text())
    entry = disk_data["entries"][0]
    assert "_resolved_cwd" not in entry, (
        "cmd_get must not persist _resolved_cwd to graph.json"
    )


# ---------------------------------------------------------------------------
# Task 1.2: Filing-site cwd derivation from explicit --project via work-map
# ---------------------------------------------------------------------------

def _settings_yaml_for_project(settings_path: Path, project: str, root: str) -> None:
    """Write a minimal settings.yaml mapping project -> root."""
    import textwrap
    settings_path.write_text(textwrap.dedent(f"""\
        work:
          projects:
            {project}:
              path: {root}
    """))


def test_ac2_hp_idea_explicit_project_stores_workmap_cwd(tmp_graph, tmp_path):
    """AC2-HP: idea --project <mapped> from a foreign cwd stores cwd == work-map root."""
    from unittest.mock import patch

    work_root = str(tmp_path / "mapped-root")
    settings_path = tmp_graph.parent / "settings.yaml"
    _settings_yaml_for_project(settings_path, "fno", work_root)

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    ), patch("fno.graph._intake.repo_root", return_value="/some/foreign/cwd"):
        r = _invoke("graph", "idea", "Test idea", "--project", "fno")

    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    assert len(entries) == 1
    assert entries[0]["project"] == "fno"
    assert entries[0]["cwd"] == work_root


def test_ac2_err_idea_unmapped_project_falls_back_to_repo_root(tmp_graph, tmp_path):
    """AC2-ERR: idea --project unknown-proj stores cwd == repo_root() fallback, succeeds."""
    from unittest.mock import patch

    settings_path = tmp_graph.parent / "settings.yaml"
    _settings_yaml_for_project(settings_path, "other-project", "/some/root")
    fake_repo_root = "/fake/repo/root"

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    ), patch("fno.graph._intake.repo_root", return_value=fake_repo_root):
        r = _invoke("graph", "idea", "Unknown proj idea", "--project", "unknown-proj")

    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    assert len(entries) == 1
    assert entries[0]["project"] == "unknown-proj"
    assert entries[0]["cwd"] == fake_repo_root


def test_ac2_edge_idea_explicit_cwd_wins_over_workmap(tmp_graph, tmp_path):
    """AC2-EDGE: idea --project <mapped> --cwd /explicit -> stored cwd is /explicit."""
    from unittest.mock import patch

    work_root = str(tmp_path / "mapped-root")
    settings_path = tmp_graph.parent / "settings.yaml"
    _settings_yaml_for_project(settings_path, "fno", work_root)

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    ):
        r = _invoke(
            "graph", "idea", "Explicit cwd wins",
            "--project", "fno",
            "--cwd", "/tmp/deliberate",
        )

    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    assert entries[0]["cwd"] == "/tmp/deliberate"


def test_ac2_hp_add_explicit_project_stores_workmap_cwd(tmp_graph, tmp_path):
    """AC2-HP (add): add --project <mapped> stores cwd == work-map root."""
    from unittest.mock import patch

    work_root = str(tmp_path / "add-root")
    settings_path = tmp_graph.parent / "settings.yaml"
    _settings_yaml_for_project(settings_path, "fno", work_root)

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    ), patch("fno.graph._intake.repo_root", return_value="/foreign/cwd"):
        r = _invoke("graph", "add", "Add feature", "--project", "fno")

    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    assert entries[0]["cwd"] == work_root


def test_ac2_edge_add_explicit_cwd_wins_over_workmap(tmp_graph, tmp_path):
    """AC2-EDGE: add --project <mapped> --cwd /deliberate -> stored cwd is /deliberate."""
    from unittest.mock import patch

    work_root = str(tmp_path / "add-root")
    settings_path = tmp_graph.parent / "settings.yaml"
    _settings_yaml_for_project(settings_path, "fno", work_root)

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    ):
        r = _invoke(
            "graph", "add", "Add explicit cwd",
            "--project", "fno",
            "--cwd", "/tmp/deliberate",
        )

    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    assert entries[0]["cwd"] == "/tmp/deliberate"


def test_ac2_ui_update_unmapped_project_warns_cwd_unchanged(tmp_graph, tmp_path, capsys):
    """AC2-UI: update --project unmapped (no --cwd) -> stderr warning, project updated, cwd unchanged."""
    from unittest.mock import patch

    original_cwd = "/original/cwd"
    node = {
        "id": "ab-updatetest",
        "title": "UpdateTarget",
        "project": "old-project",
        "cwd": original_cwd,
        "status": "idea",
    }
    tmp_graph.write_text(json.dumps({"entries": [node]}) + "\n")

    settings_path = tmp_graph.parent / "settings.yaml"
    _settings_yaml_for_project(settings_path, "other-project", "/some/root")

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    ):
        r = _invoke("graph", "update", "ab-updatetest", "--project", "unmapped-proj")

    assert r.exit_code == 0, r.output
    assert "cwd left unchanged" in r.output + getattr(r, "stderr", ""), (
        f"Expected 'cwd left unchanged' warning; got output={r.output!r}"
    )
    entries = _read_graph(tmp_graph)
    updated = next(e for e in entries if e["id"] == "ab-updatetest")
    assert updated["project"] == "unmapped-proj", "project must be updated"
    assert updated["cwd"] == original_cwd, "cwd must be unchanged when unmapped"


def test_ac2_fr_update_mapped_project_derives_cwd(tmp_graph, tmp_path):
    """AC2-FR: update --project <mapped> (no --cwd) -> stored cwd becomes work-map root."""
    from unittest.mock import patch

    work_root = str(tmp_path / "update-root")
    settings_path = tmp_graph.parent / "settings.yaml"
    _settings_yaml_for_project(settings_path, "fno", work_root)

    node = {
        "id": "ab-updatetest2",
        "title": "UpdateMapped",
        "project": "old-project",
        "cwd": "/old/cwd",
        "status": "idea",
    }
    tmp_graph.write_text(json.dumps({"entries": [node]}) + "\n")

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    ):
        r = _invoke("graph", "update", "ab-updatetest2", "--project", "fno")

    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    updated = next(e for e in entries if e["id"] == "ab-updatetest2")
    assert updated["project"] == "fno"
    assert updated["cwd"] == work_root


def test_ac2_update_explicit_cwd_wins_over_workmap(tmp_graph, tmp_path):
    """AC2-EDGE: update --project <mapped> --cwd /explicit -> explicit cwd stored, no warning."""
    from unittest.mock import patch

    work_root = str(tmp_path / "update-root2")
    settings_path = tmp_graph.parent / "settings.yaml"
    _settings_yaml_for_project(settings_path, "fno", work_root)

    node = {
        "id": "ab-updatetest3",
        "title": "UpdateExplicitCwd",
        "project": "old-project",
        "cwd": "/old/cwd",
        "status": "idea",
    }
    tmp_graph.write_text(json.dumps({"entries": [node]}) + "\n")

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    ):
        r = _invoke(
            "graph", "update", "ab-updatetest3",
            "--project", "fno",
            "--cwd", "/explicit/override",
        )

    assert r.exit_code == 0, r.output
    assert "cwd left unchanged" not in (r.output or "")
    entries = _read_graph(tmp_graph)
    updated = next(e for e in entries if e["id"] == "ab-updatetest3")
    assert updated["cwd"] == "/explicit/override"


def test_ac2_new_explicit_project_unscoped_derives_cwd(tmp_graph, tmp_path):
    """AC2: new --project <mapped> --unscoped -> cwd derived from work-map despite --unscoped."""
    from unittest.mock import patch

    work_root = str(tmp_path / "new-root")
    settings_path = tmp_graph.parent / "settings.yaml"
    _settings_yaml_for_project(settings_path, "fno", work_root)

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    ):
        r = _invoke(
            "graph", "new", "New unscoped with explicit project",
            "--project", "fno",
            "--unscoped",
            "--force-domain",
        )

    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    assert len(entries) == 1
    assert entries[0]["project"] == "fno"
    assert entries[0]["cwd"] == work_root


def test_ac2_new_no_project_unchanged(tmp_graph, tmp_path):
    """AC2: new without --project keeps existing behavior (cwd from git root or None)."""
    from unittest.mock import patch

    settings_path = tmp_graph.parent / "settings.yaml"
    _settings_yaml_for_project(settings_path, "fno", "/some/mapped/root")

    with patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    ), patch("fno.graph._intake.resolve_git_roots", return_value=("myrepo", "/git/root")):
        r = _invoke(
            "graph", "new", "New without project flag",
            "--force-domain",
        )

    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    assert len(entries) == 1
    assert entries[0]["cwd"] == "/git/root"


# --- US3: per-node dispatch verb + brief -----------------------------------


def test_update_dispatch_verb_and_brief_write(tmp_graph):
    r = _invoke("graph", "add", "Verb node")
    nid = json.loads(r.output)["id"]
    r2 = _invoke("graph", "update", nid, "--dispatch-verb", "/think",
                 "--dispatch-brief", "brainstorm the retry design")
    assert r2.exit_code == 0, r2.output
    node = _read_graph(tmp_graph)[0]
    assert node["dispatch_verb"] == "/think"
    assert node["dispatch_brief"] == "brainstorm the retry design"


def test_update_dispatch_verb_null_clears(tmp_graph):
    r = _invoke("graph", "add", "Verb node")
    nid = json.loads(r.output)["id"]
    _invoke("graph", "update", nid, "--dispatch-verb", "/think")
    _invoke("graph", "update", nid, "--dispatch-verb", "null")
    assert _read_graph(tmp_graph)[0]["dispatch_verb"] is None


def test_dispatch_fields_default_absent(tmp_graph):
    """A node with no dispatch overrides carries null verb/brief (built-in path)."""
    r = _invoke("graph", "add", "Plain node")
    nid = json.loads(r.output)["id"]
    node = next(n for n in _read_graph(tmp_graph) if n["id"] == nid)
    assert node.get("dispatch_verb") is None
    assert node.get("dispatch_brief") is None


# ---------------------------------------------------------------------------
# G1+G2 guarded selection + starvation receipts + triage pile (x-3236)
# ---------------------------------------------------------------------------


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}))


def test_next_excludes_stale_ready_with_receipt(tmp_graph):
    _seed(tmp_graph, [{
        "id": "ab-stale", "title": "abandoned", "project": "fno",
        "plan_path": "/nonexistent/plan.md", "priority": "p2",
        "created_at": "2026-01-01T00:00:00+00:00",  # ~200d before real now -> stale
    }])
    r = _invoke("backlog", "next", "--project", "fno")
    assert "null" in r.output
    assert "excluded ab-stale: quarantined" in r.output


def test_next_excludes_dead_ancestor_child_with_receipt(tmp_graph):
    from datetime import datetime, timezone, timedelta

    recent = (datetime.now(timezone.utc) - timedelta(days=2)).isoformat()
    _seed(tmp_graph, [
        {"id": "ab-epic", "title": "epic", "project": "fno",
         "superseded_by": "ab-new"},
        {"id": "ab-child", "title": "child", "project": "fno",
         "parent": "ab-epic", "plan_path": "/nonexistent/plan.md",
         "created_at": recent, "priority": "p2"},
    ])
    r = _invoke("backlog", "next", "--project", "fno")
    assert "null" in r.output
    assert "excluded ab-child: dead-ancestor" in r.output


def test_next_selects_healthy_ready_node(tmp_graph):
    from datetime import datetime, timezone, timedelta

    recent = (datetime.now(timezone.utc) - timedelta(days=1)).isoformat()
    plan = tmp_graph.parent / "p.md"
    plan.write_text("---\ntitle: t\n---\n")
    _seed(tmp_graph, [{
        "id": "ab-live", "title": "live", "project": "fno",
        "plan_path": str(plan), "created_at": recent, "priority": "p2",
    }])
    r = _invoke("backlog", "next", "--project", "fno")
    assert '"id": "ab-live"' in r.output


def test_triage_pile_lists_deferred_oldest_first(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-a", "title": "a", "project": "fno",
         "deferred_at": "2026-01-01T00:00:00+00:00",
         "deferred_reason": "stale-quarantine (guard)", "priority": "p2"},
        {"id": "ab-b", "title": "b", "project": "fno",
         "deferred_at": "2026-06-01T00:00:00+00:00",
         "deferred_reason": "manual pause", "priority": "p1"},
    ])
    r = _invoke("backlog", "triage", "pile", "--json")
    data = json.loads(r.stdout)
    ids = [x["id"] for x in data["pile"]]
    assert ids == ["ab-a", "ab-b"]  # oldest-deferred leads
    assert data["pile"][0]["reason"] == "stale-quarantine (guard)"


def test_triage_pile_empty_when_no_deferred(tmp_graph):
    _seed(tmp_graph, [{"id": "ab-x", "title": "x", "project": "fno"}])
    r = _invoke("backlog", "triage", "pile")
    assert "triage pile empty" in r.output


def test_maintain_apply_defers_stale_ready(tmp_graph):
    _seed(tmp_graph, [{
        "id": "ab-old", "title": "old", "project": "fno",
        "plan_path": "/nonexistent/plan.md", "priority": "p2",
        "created_at": "2026-01-01T00:00:00+00:00",
    }])
    r = _invoke("backlog", "maintain", "--apply", "--json")
    data = json.loads(r.stdout)
    applied = [x["node_id"] for x in data["stale_ready"]["applied"]]
    assert "ab-old" in applied
    # Reversible defer landed with the quarantine reason.
    entries = _read_graph(tmp_graph)
    node = next(e for e in entries if e["id"] == "ab-old")
    assert node["deferred_reason"] == "stale-quarantine (guard)"


def test_next_mission_receipts_ignore_other_mission(tmp_graph):
    # A --mission scoped next that returns null must not explain itself with a
    # node from a different mission (codex P2).
    _seed(tmp_graph, [
        {"id": "ab-otherm", "title": "other", "project": "fno",
         "mission_id": "mission-Y", "priority": "p2"},  # plan-less, mission Y
    ])
    r = _invoke("backlog", "next", "--project", "fno", "--mission", "mission-X")
    assert "null" in r.output
    assert "ab-otherm" not in r.output  # out-of-mission node never reported


def test_maintain_apply_skips_in_review_node(tmp_graph):
    # An old ready node that already carries a PR is in-review (movement); the
    # stale-ready leg must never defer it into the pile.
    _seed(tmp_graph, [{
        "id": "ab-inrev", "title": "in review", "project": "fno",
        "plan_path": "/nonexistent/plan.md", "priority": "p2",
        "created_at": "2026-01-01T00:00:00+00:00", "pr_number": 42,
    }])
    r = _invoke("backlog", "maintain", "--apply", "--json")
    data = json.loads(r.stdout)
    applied = [x["node_id"] for x in data["stale_ready"]["applied"]]
    assert "ab-inrev" not in applied
    entries = _read_graph(tmp_graph)
    node = next(e for e in entries if e["id"] == "ab-inrev")
    assert not node.get("deferred_at")  # never quarantined


def test_ready_excludes_stale_and_dead_ancestor(tmp_graph):
    # `ready` feeds lane-fill / the daemon / --all-ready dispatch, so it must
    # apply the SAME guard as `next` (code-reviewer finding, x-3236).
    from datetime import datetime, timezone, timedelta

    recent = (datetime.now(timezone.utc) - timedelta(days=1)).isoformat()
    _seed(tmp_graph, [
        {"id": "ab-live0", "title": "live", "project": "fno", "status": "ready",
         "plan_path": "/no/p.md", "created_at": recent, "priority": "p2"},
        {"id": "ab-stale0", "title": "stale", "project": "fno", "status": "ready",
         "plan_path": "/no/p.md", "created_at": "2026-01-01T00:00:00+00:00",
         "priority": "p2"},
        {"id": "ab-deadep", "title": "epic", "project": "fno",
         "superseded_by": "ab-new"},
        {"id": "ab-deadch", "title": "child", "project": "fno", "status": "ready",
         "parent": "ab-deadep", "plan_path": "/no/p.md", "created_at": recent,
         "priority": "p2"},
    ])
    ids = [e["id"] for e in json.loads(_invoke("backlog", "ready", "--project", "fno").stdout)]
    assert "ab-live0" in ids
    assert "ab-stale0" not in ids      # stale-quarantined
    assert "ab-deadch" not in ids      # dead-ancestor
