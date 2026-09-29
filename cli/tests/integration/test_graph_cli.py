"""Integration tests for `fno graph` subcommands via the typer CLI.

Each test verifies behavior matches the legacy roadmap-tasks.py script.
Uses typer.testing.CliRunner for speed; a seeded temporary store keeps live
state out of the test process.
"""
from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from fno.cli import app
from tests.fixtures.graph_seed import seed_graph

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
    """A fresh graph store; its graph.json path is only the stable anchor."""
    g = tmp_path / "graph.json"
    # The native door resolves the anchor through the layout ladder, and a
    # fresh root would answer the db/ spelling. The empty legacy twin makes
    # the ladder answer this root spelling, so every side of the seam reads
    # one store.
    import sqlite3

    sqlite3.connect(tmp_path / "graph.db").close()
    # Patch the module-level constants so all operations hit this temp file
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    # Also patch the store module's imported names
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    # Seam readers (sidecar projection, guarded metadata/display reads)
    # resolve through paths.graph_json at call time; pin the resolver too.
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    # The native binary (which the get read-backs exec) resolves the store
    # through FNO_CONFIG's state_dir; point it at this same tmp dir so both
    # sides of the seam read one store.
    (tmp_path / "config.toml").write_text(f'state_dir = "{tmp_path}"\n')
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / "config.toml"))
    return g


def _native_get(*args) -> str:
    """Read one node back through the native binary (the graph-mode get
    ladder is the binary's now). Returns captured stdout; asserts exit 0."""
    import os as _os
    import subprocess as _sp

    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    proc = _sp.run(
        [str(binary), "backlog", "get", *args],
        capture_output=True,
        text=True,
        env={**_os.environ, "FNO_TRACKER_BACKEND": "graph"},
    )
    assert proc.returncode == 0, proc.stderr
    return proc.stdout


def _native_get_raw(*args):
    """The exit-code-visible shape of the same read: (code, stdout, stderr)."""
    import os as _os
    import subprocess as _sp

    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    proc = _sp.run(
        [str(binary), "backlog", "get", *args],
        capture_output=True,
        text=True,
        env={**_os.environ, "FNO_TRACKER_BACKEND": "graph"},
    )
    return proc.returncode, proc.stdout, proc.stderr


def _seed_graph_text(path: Path, payload: str, **_kwargs) -> None:
    seed_graph(path, payload)


def _invoke(*args, input=None):
    """Invoke the fno CLI and return the result."""
    return runner.invoke(app, list(args), input=input, catch_exceptions=False)


def _door_graph(g: Path, *args: str) -> _NativeResult:
    """The native door against the fixture's store (selection verbs answer
    natively now). Claims stay hermetic via FNO_CLAIMS_ROOT and the roster
    stub answers an empty listing, so the fleet read never leaves the sandbox."""
    from tests.goldens._door import door, roster_stub

    code, out, err = door(g.parent, list(args), path_prepend=roster_stub(g.parent, []))
    return _NativeResult(code, out, err)


class _NativeResult:
    """The CliRunner-shaped face of a native-door run."""

    def __init__(self, code: int, out: str, err: str):
        self.exit_code = code
        self.output = out
        self.stdout = out
        self.stderr = err


def _native_verb(verb: str, *args: str) -> _NativeResult:
    """The native backlog door: execs the binary against the same store the
    fixture wired through FNO_CONFIG. The difficulty default rides only the
    create verbs, where the retired add shim auto-appended it."""
    from tests._native_door import run_native

    argv = list(args)
    if verb in ("add", "idea") and "--difficulty" not in argv:
        argv.extend(["--difficulty", "medium"])
    code, out, err = run_native("backlog", verb, *argv)
    return _NativeResult(code, out, err)


def _native_session(*args: str) -> _NativeResult:
    """The Rust-owned session lifecycle door, migrated out of the Python app."""
    from tests._native_door import run_native

    code, out, err = run_native("backlog", "session", *args)
    return _NativeResult(code, out, err)


def _read_graph(g: Path) -> list[dict]:
    # The store owns state now; graph.json is a frozen export mirror, so a
    # post-command read-back must come from the store, not the file.
    from fno.graph.store import read_graph_strict

    return read_graph_strict(g)


def test_session_reap_open_returns_positive_settled_receipt(tmp_graph):
    """AC3: observer reap fills the exact open row and reads it back.

    Seeded in_progress with nothing else open: the settle rolls the node off
    in_progress (the x-9657 red), so status_after reads idea."""
    _seed_graph_text(tmp_graph, json.dumps({
        "entries": [{
            "id": "x-reap0001",
            "title": "Reap me",
            "status": "in_progress",
            "sessions": [{
                "phase": "execute",
                "harness": "codex",
                "session_id": "dead-session",
                "started_at": "2026-08-20T00:00:00Z",
            }],
        }]
    }) + "\n")

    result = _native_session(
        "reap-open", "x-reap0001",
        "--harness", "codex", "--session-id", "dead-session", "--json",
    )

    assert result.exit_code == 0, result.output
    receipt = json.loads(result.output)
    assert receipt["settled"] is True
    assert receipt["row_removed"] is False
    assert receipt["row_closed"] is True
    assert receipt["status_after"] == "idea"
    assert receipt["remaining_open_do"] == 0
    saved = _read_graph(tmp_graph)[0]
    assert saved["sessions"][0]["ended_at"], "the settled row is filled, never erased"
    assert saved["status"] == "idea"


def test_session_reap_open_without_node_settles_every_node_holding_the_identity(tmp_graph):
    """The death-cascade form: no node named, every node with an open row
    for the identity settles and node_ids names them all."""
    _seed_graph_text(tmp_graph, json.dumps({
        "entries": [
            {
                "id": "x-reap0002",
                "title": "First holder",
                "sessions": [{
                    "phase": "ship",
                    "harness": "codex",
                    "session_id": "dead-session",
                    "started_at": "2026-08-20T00:00:00Z",
                }],
            },
            {
                "id": "x-reap0003",
                "title": "Second holder",
                "sessions": [{
                    "phase": "review",
                    "harness": "codex",
                    "session_id": "dead-session",
                    "started_at": "2026-08-20T00:00:00Z",
                }],
            },
            {
                "id": "x-reap0004",
                "title": "Other session",
                "sessions": [{
                    "phase": "ship",
                    "harness": "codex",
                    "session_id": "alive-session",
                    "started_at": "2026-08-20T00:00:00Z",
                }],
            },
        ]
    }) + "\n")

    result = _native_session(
        "reap-open",
        "--harness", "codex", "--session-id", "dead-session", "--phase", "all", "--json",
    )

    assert result.exit_code == 0, result.output
    receipt = json.loads(result.output)
    assert receipt["settled"] is True
    assert sorted(receipt["node_ids"]) == ["x-reap0002", "x-reap0003"]
    assert receipt["row_closed"] is True
    saved = {e["id"]: e for e in _read_graph(tmp_graph)}
    assert all(
        row.get("ended_at")
        for node in ("x-reap0002", "x-reap0003")
        for row in saved[node]["sessions"]
    )
    # Store rows normalize an open session to ended_at=None; open is a value now, not a missing key.
    assert saved["x-reap0004"]["sessions"][0].get("ended_at") is None


def test_configured_prefix_mint_and_resolve(tmp_graph, monkeypatch):
    """ab-bbfccb8f end-to-end: a configured prefix/width mints configured-format
    ids (US2: ``xy-`` + 4 hex) that then resolve through the CLI verbs (US4:
    ``update`` would have hard-errored under the old ``startswith('ab-')`` gate)."""
    import re

    from fno.config import SettingsModel

    model = SettingsModel(config={"backlog": {"id_prefix": "xy-", "id_hex_width": 4}})
    monkeypatch.setattr("fno.config.load_settings", lambda: model)
    # The mint is native: the same override rides the fixture config.toml the
    # binary reads, and the get half below reads the Python side.
    cfg = tmp_graph.parent / "config.toml"
    cfg.write_text(cfg.read_text() + '\n[backlog]\nid_prefix = "xy-"\nid_hex_width = 4\n')

    r = _native_verb("add", "Configured Feature")
    assert r.exit_code == 0, r.output + r.stderr
    nid = json.loads(r.output)["id"]
    assert re.fullmatch(r"xy-[0-9a-f]{4}", nid), nid

    r2 = _invoke("backlog", "get", nid)
    assert r2.exit_code == 0, r2.output


def test_difficulty_prompt_value_proc_reasks_on_bad_band():
    """A bad band at the difficulty prompt raises click.UsageError (click's
    re-ask signal); the old bare ValueError surfaced as a traceback instead."""
    import click
    import pytest as _pytest

    import fno.graph.cli as gcli

    with _pytest.raises(click.UsageError):
        gcli._prompt_difficulty_value("hard")
    assert gcli._prompt_difficulty_value("high") == "high"
def test_legacy_id_resolves_under_configured_install(tmp_graph, monkeypatch):
    """AC3-HP/AC3-EDGE: a configured install still resolves a historical ab- id
    (mixed-format graph), because resolution honors both the configured and the
    legacy prefix."""
    # Seed a legacy 8-hex node directly.
    g = tmp_graph
    data = {"entries": _read_graph(g)}
    data["entries"].append({
        "id": "ab-55ba9adb", "title": "Legacy node", "priority": "p2",
        "status": "ready", "blocked_by": [], "type": "feature",
    })
    _seed_graph_text(g, json.dumps(data))

    from fno.config import SettingsModel
    model = SettingsModel(config={"backlog": {"id_prefix": "xy-", "id_hex_width": 4}})
    monkeypatch.setattr("fno.config.load_settings", lambda: model)

    r = _invoke("backlog", "get", "ab-55ba9adb")
    assert r.exit_code == 0, r.output


# --- next ---

def test_ac1_hp_graph_next_empty(tmp_graph):
    """AC1-HP: fno graph next on empty graph returns null."""
    r = _door_graph(tmp_graph, "next", "--all")
    assert r.exit_code == 0, r.output
    assert r.output.strip() == "null"


def test_ac1_hp_graph_next_returns_highest_priority(tmp_graph):
    """AC1-HP: fno graph next picks highest priority.

    `graph add` creates plan-less nodes (idea status), so `next` needs
    `--include-ideas` to consider them. The default exclusion behavior
    is covered separately in test_graph_status.py.
    """
    _native_verb("add", "Low", "--priority", "p3")
    _native_verb("add", "High", "--priority", "p1")
    r = _door_graph(tmp_graph, "next", "--all", "--include-ideas")
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
    _native_verb("add", "Feature 1")
    r = _invoke("backlog", "ready", "--all", "--include-ideas")
    assert r.exit_code == 0
    data = json.loads(r.output)
    assert isinstance(data, list)
    assert len(data) == 1
    assert data[0]["title"] == "Feature 1"


def test_ac1_hp_undispatched_names_known_node_and_scans_entries(tmp_graph):
    _seed_graph_text(tmp_graph, json.dumps({
        "entries": [{
            "id": "x-known-undispatched",
            "title": "Known",
            "status": "ready",
            "plan_path": "/plans/known.md",
            "priority": "p0",
            "domain": "code",
            "blocked_by": [],
        }]
    }) + "\n")

    r = _door_graph(tmp_graph, "undispatched", "--all", "--json")

    assert r.exit_code == 0, r.output
    data = json.loads(r.output)
    assert data["entries_scanned"] == 1
    assert any(row["id"] == "x-known-undispatched" for row in data["rows"])


def test_ac1_hp_undispatched_external_backend_uses_tracker_join(tmp_graph, monkeypatch):
    import fno.graph.cli as graph_cli

    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_graph.parent / "claims"))
    seed_graph(tmp_graph, [])
    monkeypatch.setattr(
        graph_cli,
        "_joined_open_candidates",
        lambda: [{
            "id": "x-external-undispatched",
            "status": "ready",
            "plan_path": "/plans/external.md",
            "priority": "p0",
            "domain": "code",
            "blocked_by": [],
        }],
    )

    r = _invoke("backlog", "undispatched", "--all", "--json")

    assert r.exit_code == 0, r.output
    data = json.loads(r.output)
    assert any(row["id"] == "x-external-undispatched" for row in data["rows"])


def test_wheel_selection_verbs_tombstone_on_the_graph_backend(tmp_graph, monkeypatch):
    """The wheel keeps only the external-tracker bodies: on the graph backend
    both selection verbs refuse, naming the native door (exit 2)."""
    monkeypatch.delenv("FNO_TRACKER_BACKEND", raising=False)
    seed_graph(tmp_graph, [])

    r = _invoke("backlog", "next", "--all")
    assert r.exit_code == 2, r.output
    assert "the selection is served by the native door" in r.output

    r = _invoke("backlog", "undispatched", "--all", "--json")
    assert r.exit_code == 2, r.output
    assert "the observer is served by the native door" in r.output


# --- get ---

def test_ac1_hp_graph_get_returns_node(tmp_graph):
    """AC1-HP: fno graph get returns full node JSON."""
    r = _native_verb("add", "GetTarget")
    node_id = json.loads(r.output)["id"]

    data = json.loads(_native_get(node_id))
    assert data["id"] == node_id
    assert data["title"] == "GetTarget"


def test_ac2_err_graph_get_unknown_exits_nonzero(tmp_graph):
    """AC2-ERR: fno graph get unknown ID exits 1."""
    code, _, _ = _native_get_raw("ab-deadbeef")
    assert code != 0
def test_queue_accepts_multiple_ids_space_and_comma_separated(tmp_graph):
    """fno backlog queue ab-X,ab-Y ab-Z queues all three atomically."""
    ids = []
    for title in ("Multi-A", "Multi-B", "Multi-C"):
        r = _native_verb("add", title)
        # raw_decode tolerates trailing stderr (Multi-B/C resemble Multi-A, so the
        # filing-time dedup receipt mixes into r.output via CliRunner).
        ids.append(json.JSONDecoder().raw_decode(r.output)[0]["id"])
    # Mix comma and space separators.
    r = _native_verb("queue", f"{ids[0]},{ids[1]}", ids[2], "--reason", "batch")
    assert r.exit_code == 0, r.output + r.stderr
    queued_ids = {x["id"] for x in json.loads(_native_verb("queued").output)}
    assert queued_ids == set(ids)
    # Same reason on all three.
    for tid in ids:
        data = json.loads(_native_get(tid))
        assert data["queued_reason"] == "batch"


def test_queue_batch_is_atomic_on_unknown_id(tmp_graph):
    """If any ID is unknown, no nodes are queued."""
    r = _native_verb("add", "Real")
    real_id = json.loads(r.output)["id"]
    r = _native_verb("queue", f"{real_id},ab-deadbeef")
    assert r.exit_code != 0
    # Real node was NOT queued because the batch aborted.
    data = json.loads(_native_get(real_id))
    assert data.get("queued_at") is None


def test_unqueue_accepts_multiple_ids(tmp_graph):
    ids = []
    for title in ("UnqA", "UnqB"):
        r = _native_verb("add", title)
        ids.append(json.loads(r.output)["id"])
    _native_verb("queue", ids[0])
    _native_verb("queue", ids[1])
    r = _native_verb("unqueue", f"{ids[0]},{ids[1]}")
    assert r.exit_code == 0, r.stderr
    queued_listing = json.loads(_native_verb("queued").output)
    assert queued_listing == []


def test_done_clears_queued_state(tmp_graph):
    r = _native_verb("add", "QueuedThenDone")
    nid = json.loads(r.output)["id"]
    _native_verb("queue", nid)
    # Evidence lands on the row first; the canonical bare close's mutation is
    # what clears the queued ghost fields, and the subject of this test is
    # that clear, not the note path.
    data = {"entries": _read_graph(tmp_graph)}
    next(e for e in data["entries"] if e["id"] == nid)["completion_note"] = (
        "queued-state fixture"
    )
    _seed_graph_text(tmp_graph, json.dumps(data))
    # The bare close is native; the queued-ghost clear is its mutation.
    _native_verb("done", nid)
    data = json.loads(_native_get(nid))
    assert data.get("queued_at") is None
    assert data["completed_at"] is not None


def test_done_audit_tags_operator_when_driving(tmp_graph, monkeypatch):
    """cv-9def52a7: `done` during a drive window emits backlog_done_operator_initiated."""
    from fno import drive_authority as da

    captured: dict = {}
    monkeypatch.setattr(da, "is_drive_authority_active", lambda *a, **k: True)
    monkeypatch.setattr(
        da,
        "emit_operator_initiated",
        lambda action_type, **kw: captured.update(type=action_type, kw=kw),
    )
    nid = json.loads(_native_verb("add", "DriveDone").output)["id"]
    _invoke("backlog", "done", nid, "--note", "drive fixture")
    assert captured.get("type") == "backlog_done_operator_initiated"
    assert captured["kw"]["task_id"] == nid
    assert captured["kw"]["source"] == "backlog"


def test_done_no_audit_tag_when_not_driving(tmp_graph, monkeypatch):
    """No drive window -> done does not emit the operator-initiated tag."""
    from fno import drive_authority as da

    calls = {"n": 0}
    monkeypatch.setattr(da, "is_drive_authority_active", lambda *a, **k: False)
    monkeypatch.setattr(
        da, "emit_operator_initiated", lambda *a, **k: calls.update(n=calls["n"] + 1)
    )
    nid = json.loads(_native_verb("add", "NoDriveDone").output)["id"]
    _invoke("backlog", "done", nid, "--note", "no-drive fixture")
    assert calls["n"] == 0


# --- view ---

def test_ac1_hp_graph_view_renders_html_and_prints_path(tmp_graph, tmp_path, monkeypatch):
    """AC1-HP: fno graph view rerenders HTML and echoes the path."""
    monkeypatch.setenv("FNO_NO_OPEN", "1")
    html_path = tmp_path / "graph.html"

    _native_verb("add", "ViewTarget")
    r = _invoke("backlog", "view")
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

    r = _invoke("backlog", "view")
    assert r.exit_code == 0, r.output
    assert html_path.exists()
    assert "<html" in html_path.read_text(encoding="utf-8")


# --- tree ---

# --- status ---

def test_ac1_hp_graph_status(tmp_graph):
    """AC1-HP: fno graph status shows progress summary."""
    _native_verb("add", "Feature A", "--project", "test-proj")
    r = _invoke("backlog", "status", "--all")
    assert r.exit_code == 0
    assert "test-proj" in r.output


class _SnapshotFakeTracker:
    """A NodeTracker fake carrying sentinels distinct from any graph value."""

    name = "fake-external"

    def __init__(self):
        from fno.tracker.types import TrackerCandidate, TrackerState

        self._TrackerCandidate = TrackerCandidate
        self._TrackerState = TrackerState

    def read(self, id):
        if id == "EXT-done":
            return self._TrackerCandidate(
                id=id, title="Closed blocker", state=self._TrackerState.closed
            )
        from fno.tracker.types import NodeNotFound

        raise NodeNotFound(id)

    def list_open(self):
        T, S = self._TrackerCandidate, self._TrackerState
        return [
            T(id="EXT-1", title="Free work", state=S.open, priority="p1",
              created_at="2026-01-02T00:00:00Z", blocked_by=["EXT-done"]),
            T(id="EXT-2", title="Waiting", state=S.open, priority="p2",
              created_at="2026-01-03T00:00:00Z"),
        ]

    def close(self, id):
        raise AssertionError("close is not part of the snapshot read path")


# --- validate ---

# --- cost ---

def test_ac1_hp_graph_cost(tmp_graph):
    """AC1-HP: fno graph cost records session cost (#23).

    Replaces the substring-on-stdout assertion with a state round-trip
    through `graph get`. A CLI text-format regression should not mask
    a missing or wrong-value cost write.
    """
    r = _native_verb("add", "Costly")
    node_id = json.loads(r.output)["id"]

    r = _invoke("backlog", "cost", node_id, "--session", "sess-001", "--amount", "1.50")
    assert r.exit_code == 0

    # State round-trip (#23): the cost write must be visible via
    # `graph get`. The cost_usd field aggregates across sessions and
    # cost_sessions records the individual session attribution.
    data = json.loads(_native_get(node_id))
    assert data["cost_usd"] == pytest.approx(1.50)
    cost_sessions = data.get("cost_sessions") or []
    assert any(s.get("session_id") == "sess-001" for s in cost_sessions), (
        f"sess-001 should appear in cost_sessions, got {cost_sessions!r}"
    )


# --- briefs ---

# --- remove ---

def test_ac1_hp_graph_remove(tmp_graph):
    """AC1-HP: fno graph remove deletes a node."""
    r = _native_verb("add", "ToRemove")
    node_id = json.loads(r.output)["id"]

    r = _invoke("backlog", "remove", node_id, "--force")
    assert r.exit_code == 0

    code, _, _ = _native_get_raw(node_id)
    assert code != 0


# --- defer ---

@pytest.mark.usefixtures("native_backlog_door")
def test_ac1_hp_graph_defer(tmp_graph):
    """AC1-HP: fno graph defer sets deferred_at + deferred_reason and derives status: deferred."""
    r = _native_verb("add", "ToDefer")
    node_id = json.loads(r.output)["id"]

    r = _invoke("backlog", "defer", node_id, "--reason", "stale spec")
    assert r.exit_code == 0

    data = json.loads(_native_get(node_id))
    assert data.get("deferred_at"), "deferred_at should be set to an ISO timestamp"
    assert data.get("deferred_reason") == "stale spec"
    assert data.get("status") == "deferred"
    assert not data.get("completed_at"), "completed_at must remain clear when deferring"


# --- reprioritize ---

def test_ac1_hp_graph_reprioritize(tmp_graph):
    """AC1-HP: fno graph reprioritize changes priority."""
    r = _native_verb("add", "ToRepri")
    node_id = json.loads(r.output)["id"]

    r = _invoke("backlog", "reprioritize", node_id, "p1")
    assert r.exit_code == 0

    data = json.loads(_native_get(node_id))
    assert data["priority"] == "p1"


# --- rank (ab-95a4a479: curated intra-lane ordering) ---

def _add(title: str, *, project: str, priority: str) -> str:
    r = _native_verb("add", title, "--project", project, "--priority", priority)
    assert r.exit_code == 0, r.output
    return json.loads(r.output)["id"]


def _rank_of(g: Path, node_id: str):
    for e in _read_graph(g):
        if e["id"] == node_id:
            return e.get("rank")
    raise AssertionError(f"{node_id} not in graph")


def _rank(*args: str):
    """The native rank door; the tmp_graph fixture's FNO_CONFIG pins the store.

    Every pin passes --operator, the fence's documented escape hatch, so the
    pins are deterministic in a runner descended from an agent session; the
    fence itself is covered by test_rank_operator_only.py.
    """
    import os
    import subprocess

    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        pytest.skip("no fno-agents dev build (cargo build -p fno-agents)")
    env = {**os.environ, "FNO_TRACKER_BACKEND": "graph"}
    return subprocess.run(
        [str(binary), "backlog", "rank", *args],
        capture_output=True, text=True, env=env, timeout=120,
    )


def _render_board(g: Path) -> None:
    """The board render is the keeper's async job after a native write; render
    explicitly so the ordering assertions read the fresh pin."""
    from fno.graph.render import render_graph_md

    render_graph_md(_read_graph(g), g.parent / "graph.md")


def test_ac1_hp_rank_top_pins_to_lane_front(tmp_graph):
    """AC1-HP: `rank A --top` pins A's rank and leaves unranked peers alone."""
    a = _add("AlphaCard", project="fno", priority="p1")  # Now/fno
    b = _add("BetaCard", project="fno", priority="p1")   # Now/fno

    r = _rank(a, "--top", "--operator")
    assert r.returncode == 0, r.stderr
    assert "--top" in r.stdout and a in r.stdout

    # A is now ranked, B remains unranked.
    assert _rank_of(tmp_graph, a) is not None
    assert _rank_of(tmp_graph, b) is None


def test_ac1_ui_ranked_card_leads_lane_after_before(tmp_graph):
    """AC1-UI: --before a ranked anchor places the card ahead of it on the board."""
    a = _add("FirstCard", project="fno", priority="p1")
    b = _add("SecondCard", project="fno", priority="p1")
    assert _rank(a, "--top", "--operator").returncode == 0
    r = _rank(b, "--before", a, "--operator")
    assert r.returncode == 0, r.stderr
    assert _rank_of(tmp_graph, b) < _rank_of(tmp_graph, a)
    _render_board(tmp_graph)
    md = (tmp_graph.parent / "graph.md").read_text()
    now_body = md.split("## Now", 1)[1].split("\n## ", 1)[0]
    assert now_body.index("SecondCard") < now_body.index("FirstCard")


def test_rank_child_defaults_to_within_epic_scope(tmp_graph):
    """A live-epic child ranks among its epic siblings; the receipt names the epic.

    The loose anchor's rank (1.0) must NOT enter the midpoint arithmetic: with
    the sibling at 5.0 and no ranked lower sibling, `--before` lands at 4.0
    exactly. A lane-scoped read would return (1.0 + 5.0) / 2 = 3.0.
    """
    entries = [
        {"id": "ab-epic001", "title": "Epic", "type": "epic",
         "status": "ready", "priority": "p1", "project": "fno"},
        {"id": "ab-sibl001", "title": "Ranked sibling", "status": "ready",
         "priority": "p2", "project": "fno", "parent": "ab-epic001", "rank": 5.0},
        {"id": "ab-child01", "title": "Promoted child", "status": "ready",
         "priority": "p2", "project": "fno", "parent": "ab-epic001"},
        {"id": "ab-anchor1", "title": "Now anchor", "status": "ready",
         "priority": "p1", "project": "fno", "rank": 1.0},
    ]
    _seed_graph_text(tmp_graph, json.dumps({"entries": entries}) + "\n")

    result = _rank("ab-child01", "--before", "ab-sibl001", "--operator")

    assert result.returncode == 0, result.stderr
    assert "epic ab-epic001" in result.stdout
    assert _rank_of(tmp_graph, "ab-child01") == 4.0
    assert _rank_of(tmp_graph, "ab-child01") < _rank_of(tmp_graph, "ab-sibl001")


def test_rank_child_anchor_outside_epic_refused(tmp_graph):
    """AC1-EDGE: a child cannot anchor against a loose node or another epic's child."""
    entries = [
        {"id": "ab-epic001", "title": "Epic", "type": "epic",
         "status": "ready", "priority": "p1", "project": "fno"},
        {"id": "ab-child01", "title": "Child", "status": "ready",
         "priority": "p2", "project": "fno", "parent": "ab-epic001"},
        {"id": "ab-epic002", "title": "Other epic", "type": "epic",
         "status": "ready", "priority": "p1", "project": "fno"},
        {"id": "ab-other01", "title": "Other child", "status": "ready",
         "priority": "p1", "project": "fno", "parent": "ab-epic002", "rank": 2.0},
        {"id": "ab-anchor1", "title": "Now anchor", "status": "ready",
         "priority": "p1", "project": "fno", "rank": 1.0},
    ]
    _seed_graph_text(tmp_graph, json.dumps({"entries": entries}) + "\n")

    for anchor in ("ab-anchor1", "ab-other01"):
        result = _rank("ab-child01", "--before", anchor, "--operator")
        assert result.returncode == 1, result.stderr
        assert "cross-epic rank rejected" in result.stderr
        assert "scoped to its live epic" in result.stderr
        # Refused before the locked write: no rank persisted.
        assert _rank_of(tmp_graph, "ab-child01") is None


def test_rank_within_epic_refused_without_live_epic_parent(tmp_graph):
    """AC1-EDGE: explicit --within-epic needs a live epic; loose node and
    terminal-parent child both refuse with no rank persisted."""
    entries = [
        {"id": "ab-epic003", "title": "Done epic", "type": "epic",
         "status": "done", "priority": "p1", "project": "fno"},
        {"id": "ab-child02", "title": "Terminal-parent child", "status": "ready",
         "priority": "p2", "project": "fno", "parent": "ab-epic003"},
        {"id": "ab-loose01", "title": "Loose", "status": "ready",
         "priority": "p1", "project": "fno"},
    ]
    _seed_graph_text(tmp_graph, json.dumps({"entries": entries}) + "\n")

    for target in ("ab-loose01", "ab-child02"):
        result = _rank(target, "--top", "--within-epic", "--operator")
        assert result.returncode == 1, result.stderr
        assert "--within-epic refused" in result.stderr
        assert _rank_of(tmp_graph, target) is None


def test_rank_within_epic_orders_children_on_board(tmp_graph):
    """AC1-HP end to end: a child rank reorders cards inside its epic group."""
    entries = [
        {"id": "ab-epic004", "title": "Epic", "type": "epic",
         "status": "ready", "priority": "p1", "project": "fno"},
        {"id": "ab-later1", "title": "LaterCard", "status": "ready",
         "priority": "p2", "project": "fno", "parent": "ab-epic004"},
        {"id": "ab-first1", "title": "FirstCard", "status": "ready",
         "priority": "p2", "project": "fno", "parent": "ab-epic004"},
    ]
    _seed_graph_text(tmp_graph, json.dumps({"entries": entries}) + "\n")

    r = _rank("ab-first1", "--top", "--within-epic", "--operator")
    assert r.returncode == 0, r.stderr
    assert "epic ab-epic004" in r.stdout

    _render_board(tmp_graph)
    md = (tmp_graph.parent / "graph.md").read_text()
    assert md.index("FirstCard") < md.index("LaterCard")


def test_rank_uses_in_progress_epic_board_lane(tmp_graph):
    entries = [
        {"id": "ab-epic002", "title": "Epic", "type": "epic",
         "status": "ready", "priority": "p2", "project": "fno"},
        {"id": "ab-done001", "title": "Done child", "status": "done",
         "priority": "p2", "project": "fno", "parent": "ab-epic002",
         "completed_at": "2026-01-01T00:00:00Z"},
        {"id": "ab-anchor2", "title": "Active anchor", "status": "in_progress",
         "priority": "p1", "project": "fno", "rank": 5.0},
    ]
    _seed_graph_text(tmp_graph, json.dumps({"entries": entries}) + "\n")

    result = _rank("ab-epic002", "--before", "ab-anchor2", "--operator")

    assert result.returncode == 0, result.stderr
    assert "In Progress/fno" in result.stdout
    assert _rank_of(tmp_graph, "ab-epic002") < _rank_of(tmp_graph, "ab-anchor2")


def test_ac1_after_ranked_anchor_places_behind(tmp_graph):
    """--after a ranked anchor places the card behind it (own midpoint branch)."""
    a = _add("LeadCard", project="fno", priority="p1")
    b = _add("TrailCard", project="fno", priority="p1")
    assert _rank(a, "--top", "--operator").returncode == 0
    r = _rank(b, "--after", a, "--operator")
    assert r.returncode == 0, r.stderr
    assert _rank_of(tmp_graph, b) > _rank_of(tmp_graph, a)
    _render_board(tmp_graph)
    md = (tmp_graph.parent / "graph.md").read_text()
    now_body = md.split("## Now", 1)[1].split("\n## ", 1)[0]
    assert now_body.index("LeadCard") < now_body.index("TrailCard")


def test_rank_self_anchor_rejected(tmp_graph):
    """A node cannot be ranked relative to itself (Failure Mode: self-anchor)."""
    a = _add("Solo", project="fno", priority="p1")
    assert _rank(a, "--top", "--operator").returncode == 0
    r = _rank(a, "--before", a, "--operator")
    assert r.returncode != 0
    assert "itself" in r.stderr


def test_rank_partial_id_resolves_and_guards_self(tmp_graph):
    """A partial id fuzzy-resolves; the resolved id (not the raw partial) is
    used for self-exclusion and the self-anchor guard."""
    a = _add("PartialCard", project="fno", priority="p1")
    partial = a[:7]  # 'ab-' + 4 hex, unique with a single node
    # Partial resolves and ranks the full node.
    r = _rank(partial, "--top", "--operator")
    assert r.returncode == 0, r.stderr
    assert _rank_of(tmp_graph, a) is not None
    # Partial self-anchor is still caught (resolved id == resolved anchor id).
    r2 = _rank(partial, "--after", partial, "--operator")
    assert r2.returncode != 0
    assert "itself" in r2.stderr


def test_ac1_err_cross_lane_anchor_rejected(tmp_graph):
    """AC1-ERR: --before across lanes errors naming both lanes, exits non-zero,
    and writes no rank to the target."""
    a = _add("WebCard", project="web", priority="p1")   # Now/web
    b = _add("EtlCard", project="etl", priority="p1")   # Now/etl

    r = _rank(a, "--before", b, "--operator")
    assert r.returncode != 0
    assert "Now/web" in r.stderr and "Now/etl" in r.stderr
    # No rank written to A.
    assert _rank_of(tmp_graph, a) is None


def test_ac1_edge_only_node_in_lane_bottom(tmp_graph):
    """AC1-EDGE: --bottom on the sole node in a lane succeeds with a valid rank."""
    a = _add("LonelyCard", project="fno", priority="p3")  # Later/fno (alone)
    r = _rank(a, "--bottom", "--operator")
    assert r.returncode == 0, r.stderr
    assert isinstance(_rank_of(tmp_graph, a), (int, float))


def test_rank_clear_resets_to_unranked(tmp_graph):
    """--clear returns a ranked node to the unranked flow (rank=null)."""
    a = _add("ClearMe", project="fno", priority="p1")
    assert _rank(a, "--top", "--operator").returncode == 0
    assert _rank_of(tmp_graph, a) is not None
    r = _rank(a, "--clear", "--operator")
    assert r.returncode == 0, r.stderr
    assert _rank_of(tmp_graph, a) is None


def test_rank_requires_exactly_one_flag(tmp_graph):
    a = _add("NoFlag", project="fno", priority="p1")
    assert _rank(a, "--operator").returncode != 0          # zero flags
    assert _rank(a, "--top", "--bottom", "--operator").returncode != 0  # two flags


def test_rank_nonexistent_node_errors(tmp_graph):
    r = _rank("ab-deadbeef", "--top", "--operator")
    assert r.returncode != 0
    assert "not found" in r.stderr


def test_rank_unranked_anchor_rejected(tmp_graph):
    """--before an unranked anchor errors with an actionable hint (band model:
    you position relative to other ranked cards)."""
    a = _add("AnchorMe", project="fno", priority="p1")
    b = _add("MoveMe", project="fno", priority="p1")
    r = _rank(b, "--before", a, "--operator")  # a is unranked
    assert r.returncode != 0
    assert "unranked" in r.stderr
    assert _rank_of(tmp_graph, b) is None


# --- archive ---

def test_ac1_hp_graph_archive(tmp_graph):
    """AC1-HP: fno graph archive moves done nodes (#23).

    Replaces the prior exit-code-only assertion with a state round-trip.
    The archive lives in the same store as the working graph, so both
    halves read through the store rows: the node must be GONE from the
    live rows AND PRESENT among the archived residents. Checking only
    one half would miss "moved but not removed" or the reverse.

    The sweep is now dry-run by default with a 30-day age filter, so a
    freshly-completed node needs `--apply --older-than-days 0` to move.
    """
    from fno.graph.store import commit_rows_via_store, read_archive_entries

    r = _native_verb("add", "ToArchive")
    node_id = json.loads(r.output)["id"]
    # Seed completed_at through the store rather than a CLI verb: closing is
    # merge-gated now, and archive only cares that the node reads done.
    commit_rows_via_store(tmp_graph, lambda rows: [
        {**row, "completed_at": "2026-01-01T00:00:00+00:00",
         "artifact_url": "https://example.test/artifact"} if row["id"] == node_id else row
        for row in rows
    ])

    # Dry-run default: nothing moves.
    r = _invoke("backlog", "archive")
    assert r.exit_code == 0
    assert "dry-run" in r.output
    assert read_archive_entries(tmp_graph) == []

    r = _invoke("backlog", "archive", "--apply", "--older-than-days", "0")
    assert r.exit_code == 0

    archived_ids = {e["id"] for e in read_archive_entries(tmp_graph)}
    assert node_id in archived_ids, f"{node_id} should be in archive"

    live_ids = {e["id"] for e in _read_graph(tmp_graph)}
    assert node_id not in live_ids, f"{node_id} should be removed from live graph"


# --- priority vocabulary migration (p0/p1/p2/p3) ---

def test_priority_p0_accepted(tmp_graph):
    """`backlog add "X" --priority p0` succeeds; node has priority="p0"."""
    r = _native_verb("add", "Drop everything", "--priority", "p0", "--blocks-everything")
    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    assert entries[0]["priority"] == "p0"


def test_priority_p0_requires_breaking_acknowledgment(tmp_graph):
    """AC9-ERR: p0 refuses before minting without --blocks-everything."""
    r = _native_verb("add", "Not actually broken", "--priority", "p0")
    assert r.exit_code != 0
    # The native door keeps the refusal on stderr, CliRunner mixed the streams.
    combined = (r.output or "") + (r.stderr or "")
    assert "p0 blocks everything else, usually a bug" in combined
    # The next step it names must work for whoever hit the refusal. It used to
    # say `rank --top`, which now refuses an agent; `encounter` exits 5 in the
    # bare operator shell that hits this. Both halves are on this command.
    assert "--blocks-everything" in combined
    assert "file it p1" in combined
    assert _read_graph(tmp_graph) == []


def test_new_p0_requires_breaking_acknowledgment(tmp_graph):
    """Every CLI birth path applies the p0 acknowledgment before mutation."""
    refused = _invoke("backlog", "new", "Not actually broken", "--priority", "p0")
    assert refused.exit_code != 0
    assert "p0 blocks everything else, usually a bug" in refused.output
    assert _read_graph(tmp_graph) == []

    accepted = _invoke(
        "backlog", "new", "Broken service", "--priority", "p0", "--blocks-everything"
    )
    assert accepted.exit_code == 0, accepted.output
    assert _read_graph(tmp_graph)[0]["blocks_everything"] is True


def test_priority_default_is_p2(tmp_graph):
    """`backlog add "X"` without --priority creates a node with priority="p2"."""
    r = _native_verb("add", "Default priority")
    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    assert entries[0]["priority"] == "p2"


def test_priority_migration_on_mutation(tmp_graph):
    """Legacy high/medium/low values are backfilled to p1/p2/p3 on the
    next graph mutation (recompute_statuses runs inside commit_rows_via_store).
    """
    _seed_graph_text(tmp_graph, json.dumps({
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

    # Trigger a mutation; commit_rows_via_store runs recompute_statuses
    # which contains the backfill loop.
    r = _native_verb("add", "Trigger mutation")
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
    r = _native_verb("add", "Old syntax", "--priority", "high")
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
    _seed_graph_text(tmp_graph, json.dumps({
        "entries": [
            {"id": "ab-old00001", "title": "Was high", "priority": "high",
             "plan_path": "x.md", "status": "ready",
             "created_at": "2026-01-01T00:00:00Z"},
        ]
    }))

    def legacy_row():
        return next(e for e in _read_graph(tmp_graph) if e["id"] == "ab-old00001")

    # First mutation: backfill runs.
    _native_verb("add", "Trigger 1")
    assert legacy_row()["priority"] == "p1"
    # Second mutation: the row is already on the new vocabulary; no thrash.
    _native_verb("add", "Trigger 2")
    assert legacy_row()["priority"] == "p1"


def test_priority_migration_command_is_dry_run_then_idempotent(tmp_graph):
    """AC11-HP: the migration command has explicit dry-run/apply receipts."""
    _seed_graph_text(tmp_graph, json.dumps({
        "entries": [
            {"id": "ab-old00001", "title": "Legacy", "priority": "p0"},
            {"id": "ab-old00002", "title": "Legacy 2", "priority": "p0"},
        ]
    }))
    dry = _invoke("backlog", "migrate-priorities")
    assert dry.exit_code == 0, dry.output
    assert json.loads(dry.stdout)["legacy_p0"] == 2
    assert _read_graph(tmp_graph)[0]["priority"] == "p0"
    applied = _invoke("backlog", "migrate-priorities", "--apply")
    assert json.loads(applied.stdout)["rebanded_to_p1"] == 2
    second = _invoke("backlog", "migrate-priorities", "--apply")
    assert json.loads(second.stdout)["already_migrated"] == 2
    rollback = _invoke("backlog", "migrate-priorities", "--rollback")
    assert json.loads(rollback.stdout)["restored_to_p0"] == 2
    assert all(row["priority"] == "p0" for row in _read_graph(tmp_graph))


def test_priority_missing_key_backfill(tmp_graph):
    """An entry with no priority key gets the default p2 via _apply_graph_defaults
    rather than being touched by the backfill loop (which only rewrites legacy
    string values).
    """
    _seed_graph_text(tmp_graph, json.dumps({
        "entries": [
            {"id": "ab-nokey0001", "title": "No priority key",
             "plan_path": "x.md", "status": "ready",
             "created_at": "2026-01-01T00:00:00Z"},
        ]
    }))
    _native_verb("add", "Trigger mutation")
    entries = _read_graph(tmp_graph)
    nokey = next(e for e in entries if e["id"] == "ab-nokey0001")
    assert nokey["priority"] == "p2"


def test_model_pin_rides_in_ready_and_next_json(tmp_graph):
    """x-571f US3: the model pin must ride in the `ready` and `next` JSON so the
    lane-fill (`_ready_nodes`) and sequential-drain (`fno backlog next`)
    dispatchers can thread it into the spawn they build (AC1-HP / AC2-HP)."""
    plan = _write_plan(tmp_graph.parent, "pinned.md", "Pinned")
    _seed_graph_text(tmp_graph, json.dumps({
        "entries": [
            {"id": "ab-pinned00", "title": "Pinned", "priority": "p1",
             "plan_path": str(plan), "status": "ready", "model": "fable",
             "created_at": _recent_iso(1)},
        ]
    }))

    listing = json.loads(_invoke("backlog", "ready", "--all").stdout)
    assert listing[0]["model"] == "fable"

    nxt = json.loads(_door_graph(tmp_graph, "next", "--all").stdout)
    assert nxt["model"] == "fable"


def test_dispatch_hold_is_absent_from_ready_and_next_destinations(tmp_graph, tmp_path):
    plan = tmp_path / "held.md"
    plan.write_text(
        "---\nstatus: ready\ndispatch_hold:\n"
        "  reason: Blocking review finding is unresolved\n"
        "  release_when: The finding is fixed and re-reviewed\n"
        "  review_on: 2099-08-20\n"
        "  set_by: king:119e3c52\n---\n",
        encoding="utf-8",
    )
    _seed_graph_text(tmp_graph, json.dumps({"entries": [
        {"id": "ab-5a5c", "title": "Held", "status": "ready", "priority": "p0", "plan_path": str(plan), "created_at": _recent_iso(1)},
        {"id": "ab-1a2b", "title": "Unheld", "status": "ready", "priority": "p1", "created_at": _recent_iso(1)},
    ]}))
    ready_ids = [e["id"] for e in json.loads(_invoke("backlog", "ready", "--all").stdout)]
    assert ready_ids == ["ab-1a2b"]
    assert json.loads(_door_graph(tmp_graph, "next", "--all").stdout)["id"] == "ab-1a2b"


def test_dispatch_hold_on_owner_hides_parent_and_contained_descendants(tmp_graph, tmp_path):
    plan = tmp_path / "owner-held.md"
    plan.write_text(
        "---\nstatus: ready\ndispatch_hold:\n"
        "  reason: Owner is held\n  release_when: Review passes\n"
        "  review_on: 2099-08-20\n  set_by: king\n---\n",
        encoding="utf-8",
    )
    entries = [
        {"id": "ab-5a5c", "title": "Owner", "status": "ready", "plan_path": str(plan), "created_at": _recent_iso(1)},
        {"id": "ab-1a2b", "title": "Parent child", "status": "ready", "parent": "ab-5a5c", "created_at": _recent_iso(1)},
        {"id": "ab-3c4d", "title": "Contained", "status": "ready", "contained_in": "ab-5a5c", "created_at": _recent_iso(1)},
    ]
    _seed_graph_text(tmp_graph, json.dumps({"entries": entries}))
    ready_ids = [e["id"] for e in json.loads(_invoke("backlog", "ready", "--all").stdout)]
    assert ready_ids == []
    next_result = _door_graph(tmp_graph, "next", "--all")
    assert json.loads(next_result.stdout) is None
    # The door splits the streams: starvation receipts answer on stderr.
    assert "dispatch-hold:ab-5a5c" in next_result.stderr


def test_priority_read_path_backfill(tmp_graph):
    """Read-only commands (`backlog ready`/`next`) sort correctly even before
    the first mutation triggers the on-disk backfill - `_apply_graph_defaults`
    rewrites legacy values in memory.
    """
    plan = _write_plan(tmp_graph.parent, "priority.md", "Priority")
    _seed_graph_text(tmp_graph, json.dumps({
        "entries": [
            {"id": "ab-mem00low", "title": "Was low", "priority": "low",
             "plan_path": str(plan), "status": "ready",
             "created_at": _recent_iso(2)},
            {"id": "ab-mem00hi0", "title": "Was high", "priority": "high",
             "plan_path": str(plan), "status": "ready",
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

def test_legacy_entry_without_additional_prs_loads_with_default(tmp_graph):
    """Old graph.json entries (no additional_prs key) get [] on read."""
    _seed_graph_text(tmp_graph, json.dumps({"entries": [
        {"id": "ab-12345678", "title": "Legacy", "priority": "p2",
         "type": "feature", "domain": "code", "parent": None,
         "plan_path": "x.md", "completed_at": "2026-01-01T00:00:00Z",
         "pr_number": 540, "pr_url": "https://github.com/x/y/pull/540",
         "created_at": "2026-01-01T00:00:00Z"}
    ]}))
    data = json.loads(_native_get("ab-12345678"))
    assert not data.get("additional_prs")


def test_render_html_renders_non_http_pr_url_as_plain_text(tmp_path):
    """REGRESSION (Codex P2 on PR #316), carried onto the dashboard renderer.

    A pr_url without a scheme ('github.com/x/y/pull/542') must never become an
    anchor - it would resolve as a relative link. It must also stay VISIBLE as
    escaped text; silently dropping it is the original defect.
    """
    from fno.graph.render_html import render_graph_html

    entry = {
        "id": "ab-abcdabcd", "title": "Multi", "priority": "p2",
        "type": "feature", "domain": "code", "parent": None,
        "plan_path": "x.md",
        "pr_number": 542, "pr_url": "github.com/x/y/pull/542",
        "created_at": "2026-01-01T00:00:00Z",
        # Open, not done: the static half renders only what the chips show on
        # first paint, so a closed node would exercise the payload alone and
        # leave the no-JS anchor guard untested.
        "status": "in_review",
    }
    out = tmp_path / "graph.html"
    render_graph_html([entry], out)
    html_out = out.read_text()
    assert "github.com/x/y/pull/542" in html_out, (
        "non-http url silently dropped"
    )
    assert 'href="github.com/x/y/pull/542"' not in html_out
    assert "PR #542" in html_out


def test_render_md_includes_additional_prs_on_done_nodes(tmp_graph):
    """Tree rendering surfaces additional_prs URLs for done nodes."""
    _seed_graph_text(tmp_graph, json.dumps({"entries": [
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
    from fno.graph.store import read_graph_strict
    from fno.graph.render import render_graph_md
    entries = read_graph_strict(tmp_graph)
    md_path = tmp_graph.parent / "graph.md"
    render_graph_md(entries, md_path)
    text = md_path.read_text()
    assert "https://github.com/x/y/pull/540" in text
    assert "https://github.com/x/y/pull/542" in text
    assert "wrap-up" in text
    assert "https://github.com/x/y/pull/543" in text


# --- --completion-note setter ---

def test_update_completion_note_unknown_node_errors(tmp_graph):
    """--completion-note on a missing node exits non-zero."""
    r = runner.invoke(
        app, ["backlog", "update", "ab-deadbeef", "--completion-note", "x"],
        catch_exceptions=True,
    )
    assert r.exit_code != 0


# --- note evidence: a note is a fact on the node ─────────────────────────────


def _note_node():
    node_id = json.loads(_native_verb("add", "NoteTarget").output)["id"]
    return node_id


def test_note_citing_a_contradicted_line_refuses_before_append(tmp_graph, monkeypatch):
    """AC18-ERR: exit 1, nothing appended, nothing mailed."""
    node_id = _note_node()
    monkeypatch.setattr(
        "fno.decide._evidence_gate",
        lambda payload: {
            "ok": False,
            "kind": "citation",
            "message": "cli/src/fno/law.py:99999: the file has 250 lines.",
        },
    )

    r = _invoke(
        "backlog", "note", node_id,
        "cli/src/fno/law.py:99999 is the classifier",
    )

    assert r.exit_code == 1, r.output
    node = json.loads(_native_get(node_id))
    assert not node.get("progress_notes")


def test_note_with_an_unmeasured_claim_replaces_state_and_warns(tmp_graph, monkeypatch):
    """AC19-HP: the note verb advises, never refuses a body."""
    node_id = _note_node()
    monkeypatch.setattr(
        "fno.decide._evidence_gate",
        lambda payload: {"ok": True, "rows": None, "claims": ["167 lines"]},
    )

    r = _invoke("backlog", "note", node_id, "the drain loop is 167 lines", "-q")

    assert r.exit_code == 0, r.output
    assert "unmeasured code fact" in r.stderr, r.stderr
    assert "--read" in r.stderr, r.stderr
    node = json.loads(_native_get(node_id))
    assert node["current_state"]["body"] == "the drain loop is 167 lines"


def test_note_with_a_read_stores_rows_and_prints_no_warning(tmp_graph, monkeypatch):
    """AC20-HP: executed reads land beside the state body."""
    node_id = _note_node()
    monkeypatch.setattr(
        "fno.decide._evidence_gate",
        lambda payload: {
            "ok": True,
            "rows": [
                {"cmd": "echo measured", "exit": 0, "out_head": "measured",
                 "ts": "2026-09-10T00:00:00Z", "head_sha": ""}
            ],
            "claims": None,
        },
    )

    r = _invoke(
        "backlog", "note", node_id, "advance.py is 200 lines",
        "--read", "echo measured", "--json", "-q",
    )

    assert r.exit_code == 0, r.output
    assert "unmeasured" not in r.stderr, r.stderr
    assert json.loads(r.stdout)["routed"] == "state"
    node = json.loads(_native_get(node_id))
    reads = node["current_state"]["reads"]
    assert reads[0]["cmd"] == "echo measured"
    assert reads[0]["exit"] == 0


def test_note_whose_read_failed_refuses_cleanly(tmp_graph, monkeypatch):
    """A read that cannot run is not evidence: the note refuses on the same
    ladder as a contradicted citation, never a traceback."""
    node_id = _note_node()
    monkeypatch.setattr(
        "fno.decide._evidence_gate",
        lambda payload: {
            "ok": False,
            "kind": "unmeasured",
            "message": "read 'nosuchcmd arg' did not run (exit 127) and stored no row.",
        },
    )

    r = _invoke(
        "backlog", "note", node_id, "advance.py is 200 lines",
        "--read", "nosuchcmd arg",
    )

    assert r.exit_code == 1, r.output
    assert "note refused" in r.stderr, r.stderr
    node = json.loads(_native_get(node_id))
    assert not node.get("progress_notes")


def test_quiet_still_refuses_a_contradicted_citation(tmp_graph, monkeypatch):
    """AC21-EDGE: a silent annotation is still a fact on the node."""
    node_id = _note_node()
    monkeypatch.setattr(
        "fno.decide._evidence_gate",
        lambda payload: {
            "ok": False,
            "kind": "citation",
            "message": "cli/src/fno/law.py:99999: the file has 250 lines.",
        },
    )

    r = _invoke(
        "backlog", "note", node_id,
        "cli/src/fno/law.py:99999 is the classifier",
        "--quiet",
    )

    assert r.exit_code == 1, r.output
    node = json.loads(_native_get(node_id))
    assert not node.get("progress_notes")


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
    _seed_graph_text(g, json.dumps({"entries": entries}))
    return "ab-aaaaaaaa", "ab-bbbbbbbb", "ab-cccccccc"


def test_ac3_err_unknown_subcommand_exits_nonzero():
    """AC3-ERR: fno graph bogus exits non-zero."""
    r = runner.invoke(app, ["backlog", "bogus"], catch_exceptions=True)
    assert r.exit_code != 0


# --- C3 (ab-82e65b72): epics-first selection precedence ---

def _epics_first_entries(plan_path: str):
    """Epic (p2) with a p3 ready child, plus a p0 loose node.

    Epics-first must rank the p3 epic child ahead of the p0 loose node.
    """
    return [
        {"id": "ab-epic", "title": "Epic", "type": "epic",
         "status": "ready", "priority": "p2",
         "created_at": _recent_iso(3), "project": "p", "blocked_by": [],
         "plan_path": plan_path},
        {"id": "ab-child", "title": "Child", "status": "ready", "priority": "p3",
         "created_at": _recent_iso(2), "project": "p", "parent": "ab-epic",
         "blocked_by": [], "plan_path": plan_path},
        {"id": "ab-loose", "title": "Loose", "status": "ready", "priority": "p0",
         "created_at": _recent_iso(1), "project": "p", "blocked_by": [],
         "plan_path": plan_path},
    ]


def test_graph_next_picks_epic_child_over_higher_priority_loose(tmp_graph):
    """C3: `fno graph next` selects the epic child over a p0 loose node."""
    plan = _write_plan(tmp_graph.parent, "epic-next.md", "Epic next")
    _seed_graph_text(tmp_graph, json.dumps({"entries": _epics_first_entries(str(plan))}) + "\n")
    r = _door_graph(tmp_graph, "next", "--all")
    out = json.loads(r.stdout)
    assert out is not None
    assert out["id"] == "ab-child"


def test_graph_ready_orders_epic_children_before_loose(tmp_graph):
    """C3: `fno graph ready` lists epic children ahead of loose nodes."""
    plan = _write_plan(tmp_graph.parent, "epic-order.md", "Epic order")
    _seed_graph_text(tmp_graph, json.dumps({"entries": _epics_first_entries(str(plan))}) + "\n")
    r = _invoke("backlog", "ready", "--all")
    ids = [e["id"] for e in json.loads(r.stdout)]
    assert ids.index("ab-child") < ids.index("ab-loose")


def test_graph_ready_excludes_epics(tmp_graph):
    """x-33b2 (codex P2 on PR #69): `fno backlog ready` - which the
    `dispatch-node.sh --all-ready` bulk path enumerates - must NOT list a
    container, or that path would launch a /target worker against the box.
    Shares the epic filter with `next` so the two surfaces agree."""
    plan = _write_plan(tmp_graph.parent, "epic-ready.md", "Epic ready")
    _seed_graph_text(tmp_graph, json.dumps({"entries": _epics_first_entries(str(plan))}) + "\n")
    r = _invoke("backlog", "ready", "--all")
    ids = [e["id"] for e in json.loads(r.stdout)]
    assert "ab-epic" not in ids        # the container is excluded
    assert "ab-child" in ids           # its buildable leaf is listed
    assert "ab-loose" in ids


def test_graph_next_skips_in_progress_epic_for_leaf(tmp_graph):
    """x-33b2: an IN-PROGRESS epic (one child done, one still pending) is the
    top-ranked ready node but must NOT be selected - `next` falls through to a
    buildable leaf instead of repeatedly returning the container ('it keeps
    assuming this one is next')."""
    plan = _write_plan(tmp_graph.parent, "epic-progress.md", "Epic progress")
    entries = [
        # Epic: ready, p0 -> would rank ahead of everything if selectable.
        {"id": "ab-epic", "title": "Epic", "status": "ready", "priority": "p0",
             "created_at": "2026-01-01", "project": "p", "blocked_by": [], "plan_path": str(plan)},
        # One child done, one still pending -> the epic is IN PROGRESS.
        {"id": "ab-cdone", "title": "Done child", "status": "done", "priority": "p2",
         "created_at": "2026-01-02", "project": "p", "parent": "ab-epic",
             "completed_at": "2026-01-03", "blocked_by": [], "plan_path": str(plan)},
        {"id": "ab-cpend", "title": "Pending child", "status": "ready", "priority": "p3",
         "created_at": _recent_iso(1), "project": "p", "parent": "ab-epic",
             "blocked_by": [], "plan_path": str(plan)},
    ]
    _seed_graph_text(tmp_graph, json.dumps({"entries": entries}) + "\n")
    r = _door_graph(tmp_graph, "next", "--all")
    out = json.loads(r.stdout)
    assert out is not None
    assert out["id"] != "ab-epic"      # the in-progress container is skipped
    assert out["id"] == "ab-cpend"     # its buildable pending leaf is picked


def _by_id(tmp_graph):
    return {e["id"]: e for e in _read_graph(tmp_graph)}


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
         "parent": "ab-epic0000", "blocked_by": [],
         "artifact_url": "https://example.test/artifact"},
    ]
    _seed_graph_text(tmp_graph, json.dumps({"entries": entries}) + "\n")
    r = _native_verb("done", "ab-clast002")
    assert r.exit_code == 0, r.output + r.stderr
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
         "parent": "ab-epic0000", "blocked_by": [],
         "artifact_url": "https://example.test/artifact"},
        {"id": "ab-cstill02", "title": "Child B (stays open)", "status": "ready",
         "project": "p", "parent": "ab-epic0000", "blocked_by": []},
    ]
    _seed_graph_text(tmp_graph, json.dumps({"entries": entries}) + "\n")
    r = _native_verb("done", "ab-cdone001")
    assert r.exit_code == 0, r.output + r.stderr
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
         "parent": "ab-sub00001", "blocked_by": [],
         "artifact_url": "https://example.test/artifact"},
    ]
    _seed_graph_text(tmp_graph, json.dumps({"entries": entries}) + "\n")
    r = _native_verb("done", "ab-leaf0002")
    assert r.exit_code == 0, r.output + r.stderr
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
         "parent": "ab-epic0000", "blocked_by": [],
         "artifact_url": "https://example.test/artifact"},
    ]
    _seed_graph_text(tmp_graph, json.dumps({"entries": entries}) + "\n")
    r = _native_verb("done", "ab-leaf0001")
    assert r.exit_code == 0, r.output + r.stderr
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


def test_resolved_cwd_uses_work_map_root_when_project_mapped(tmp_graph, monkeypatch, tmp_path):
    """AC1: node with project mapped in settings -> _resolved_cwd == work-map root."""
    import textwrap

    node = _make_node_with_project_cwd("myproject", "/recorded/other")
    _seed_graph_text(tmp_graph, json.dumps({"entries": [node]}) + "\n")

    # The native get resolves the work map through the settings candidates:
    # isolate the cwd and provide the map at the `<cwd>/.fno` candidate.
    fno_dir = tmp_path / ".fno"
    fno_dir.mkdir()
    (fno_dir / "settings.yaml").write_text(textwrap.dedent("""\
        work:
          workspaces:
            main:
              projects:
                - name: myproject
                  path: /mapped/root
    """))
    monkeypatch.chdir(tmp_path)

    data = json.loads(_native_get("ab-resolvetest"))
    assert data["_resolved_cwd"] == "/mapped/root", (
        f"Expected /mapped/root, got {data.get('_resolved_cwd')!r}"
    )


def test_resolved_cwd_falls_back_to_recorded_cwd_when_unmapped(tmp_graph, monkeypatch, tmp_path):
    """AC2: node with unmapped project -> _resolved_cwd == recorded cwd."""
    node = _make_node_with_project_cwd("unmapped-project", "/recorded/cwd")
    _seed_graph_text(tmp_graph, json.dumps({"entries": [node]}) + "\n")

    monkeypatch.chdir(tmp_path)
    data = json.loads(_native_get("ab-resolvetest"))
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
    _seed_graph_text(tmp_graph, json.dumps({"entries": [node]}) + "\n")

    data = json.loads(_native_get("ab-resolvetest"))
    assert data["_resolved_cwd"] == "/recorded/cwd"


def test_resolved_cwd_field_flag_works(tmp_graph, monkeypatch, tmp_path):
    """AC4: --field _resolved_cwd prints the derived value."""
    import textwrap

    node = _make_node_with_project_cwd("myproject", "/recorded/other")
    _seed_graph_text(tmp_graph, json.dumps({"entries": [node]}) + "\n")

    fno_dir = tmp_path / ".fno"
    fno_dir.mkdir()
    (fno_dir / "settings.yaml").write_text(textwrap.dedent("""\
        work:
          projects:
            myproject:
              path: /mapped/root
    """))
    monkeypatch.chdir(tmp_path)

    out = _native_get("ab-resolvetest", "--field", "_resolved_cwd")
    assert out.strip() == "/mapped/root"


def test_resolved_cwd_never_persisted_to_graph_store(tmp_graph, monkeypatch, tmp_path):
    """AC5: _resolved_cwd is never written back to the graph store."""
    node = _make_node_with_project_cwd("myproject", "/recorded/other")
    _seed_graph_text(tmp_graph, json.dumps({"entries": [node]}) + "\n")

    monkeypatch.chdir(tmp_path)
    _native_get("ab-resolvetest")

    disk_data = {"entries": _read_graph(tmp_graph)}
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
            "backlog", "new", "New unscoped with explicit project",
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
            "backlog", "new", "New without project flag",
            "--force-domain",
        )

    assert r.exit_code == 0, r.output
    entries = _read_graph(tmp_graph)
    assert len(entries) == 1
    assert entries[0]["cwd"] == "/git/root"


# --- US3: per-node dispatch verb + brief -----------------------------------


def test_update_dispatch_verb_null_clears(tmp_graph):
    r = _native_verb("add", "Verb node")
    nid = json.loads(r.output)["id"]
    _invoke("backlog", "update", nid, "--dispatch-verb", "/think")
    _invoke("backlog", "update", nid, "--dispatch-verb", "null")
    assert _read_graph(tmp_graph)[0]["dispatch_verb"] is None


def test_dispatch_fields_default_absent(tmp_graph):
    """A node with no dispatch overrides carries null verb/brief (built-in path)."""
    r = _native_verb("add", "Plain node")
    nid = json.loads(r.output)["id"]
    node = next(n for n in _read_graph(tmp_graph) if n["id"] == nid)
    assert node.get("dispatch_verb") is None
    assert node.get("dispatch_brief") is None


# ---------------------------------------------------------------------------
# G1+G2 guarded selection + starvation receipts + triage pile (x-3236)
# ---------------------------------------------------------------------------


def _seed(g: Path, entries: list[dict]) -> None:
    _seed_graph_text(g, json.dumps({"entries": entries}))


def test_next_excludes_stale_ready_with_receipt(tmp_graph):
    import os

    plan = _write_plan(tmp_graph.parent, "stale.md", "Stale")
    os.utime(plan, (0, 0))
    _seed(tmp_graph, [{
        "id": "ab-stale", "title": "abandoned", "project": "fno",
        "status": "ready",
        "plan_path": str(plan), "priority": "p2",
        "created_at": "2026-01-01T00:00:00+00:00",  # ~200d before real now -> stale
    }])
    r = _door_graph(tmp_graph, "next", "--project", "fno")
    assert "null" in r.stdout
    assert "excluded ab-stale: quarantined" in r.stderr


def test_next_excludes_dead_ancestor_child_with_receipt(tmp_graph):
    from datetime import datetime, timezone, timedelta

    recent = (datetime.now(timezone.utc) - timedelta(days=2)).isoformat()
    plan = _write_plan(tmp_graph.parent, "dead-child.md", "Dead child")
    _seed(tmp_graph, [
        {"id": "ab-epic", "title": "epic", "project": "fno",
         "superseded_by": "ab-new"},
        {"id": "ab-child", "title": "child", "project": "fno",
         "parent": "ab-epic", "plan_path": str(plan),
         "created_at": recent, "priority": "p2"},
    ])
    r = _door_graph(tmp_graph, "next", "--project", "fno")
    assert "null" in r.stdout
    assert "excluded ab-child: dead-ancestor" in r.stderr


def test_next_selects_healthy_ready_node(tmp_graph):
    from datetime import datetime, timezone, timedelta

    recent = (datetime.now(timezone.utc) - timedelta(days=1)).isoformat()
    plan = tmp_graph.parent / "p.md"
    plan.write_text("---\ntitle: t\n---\n")
    _seed(tmp_graph, [{
        "id": "ab-live", "title": "live", "project": "fno",
        "status": "ready",
        "plan_path": str(plan), "created_at": recent, "priority": "p2",
    }])
    r = _door_graph(tmp_graph, "next", "--project", "fno")
    assert '"id": "ab-live"' in r.stdout


def test_next_claims_with_lockfile_without_writing_graph_owner(tmp_graph, monkeypatch):
    from fno.claims.core import claim_status, release_claim

    recent = _recent_iso(1)
    plan = _write_plan(tmp_graph.parent, "claimed-next.md", "Claimed next")
    node_id = "ab-next0001"
    _seed_graph_text(tmp_graph, json.dumps({"entries": [{
        "id": node_id, "title": "Claimed next", "project": "fno",
        "plan_path": str(plan), "created_at": recent, "priority": "p2",
    }]}) + "\n")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_graph.parent / "claims"))
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "next-session")

    result = _door_graph(tmp_graph, "next", "--project", "fno", "--claim", "next-session")

    assert result.exit_code == 0, result.output
    assert f'"id": "{node_id}"' in result.stdout
    # The acquirer is the door process, already exited: the lockfile holds
    # with our holder and reads suspect (a dead pid never reads live).
    verdict = claim_status(f"node:{node_id}")
    assert verdict["holder"] == "next-session"
    assert verdict["state"] == "suspect"
    # The served owner is the read-time claim projection: it follows the
    # lockfile and nothing persists it. Release the claim; the owner is gone.
    from fno.graph.store import read_graph

    served = read_graph(tmp_graph)[0]
    assert served.get("locked_by") == "next-session"
    release_claim(key=f"node:{node_id}", holder="next-session")
    served_after = read_graph(tmp_graph)[0]
    assert served_after.get("locked_by") is None


def test_maintain_apply_defers_stale_ready(tmp_graph):
    _seed(tmp_graph, [
        {
            "id": "ab-old", "title": "old", "project": "fno",
            "plan_path": "/nonexistent/plan.md", "priority": "p2",
            "created_at": "2026-01-01T00:00:00+00:00",
        },
        {
            "id": "ab-folded", "title": "folded", "project": "fno",
            "contained_in": "ab-old", "created_at": _recent_iso(1),
        },
    ])
    r = _invoke("backlog", "maintain", "--apply", "--json")
    data = json.loads(r.stdout)
    applied = [x["node_id"] for x in data["stale_ready"]["applied"]]
    assert "ab-old" in applied
    # Reversible defer landed with the quarantine reason.
    entries = _read_graph(tmp_graph)
    node = next(e for e in entries if e["id"] == "ab-old")
    assert node["deferred_reason"] == "stale-quarantine (guard)"
    child = next(e for e in entries if e["id"] == "ab-folded")
    assert child["contained_in"] == "ab-old"


def test_next_mission_receipts_ignore_other_mission(tmp_graph):
    # A --mission scoped next that returns null must not explain itself with a
    # node from a different mission (codex P2).
    _seed(tmp_graph, [
        {"id": "ab-otherm", "title": "other", "project": "fno",
         "mission_id": "mission-Y", "priority": "p2"},  # plan-less, mission Y
    ])
    r = _door_graph(tmp_graph, "next", "--project", "fno", "--mission", "mission-X")
    assert "null" in r.stdout
    assert "ab-otherm" not in r.stdout + r.stderr  # out-of-mission node never reported


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
    import os
    from datetime import datetime, timezone, timedelta

    recent = (datetime.now(timezone.utc) - timedelta(days=1)).isoformat()
    plan = _write_plan(tmp_graph.parent, "ready-guards.md", "Ready guards")
    os.utime(plan, (0, 0))
    _seed(tmp_graph, [
        {"id": "ab-live0", "title": "live", "project": "fno", "status": "ready",
         "plan_path": str(plan), "created_at": recent, "priority": "p2"},
        {"id": "ab-stale0", "title": "stale", "project": "fno", "status": "ready",
         "plan_path": str(plan), "created_at": "2026-01-01T00:00:00+00:00",
         "priority": "p2"},
        {"id": "ab-deadep", "title": "epic", "project": "fno",
         "superseded_by": "ab-new"},
        {"id": "ab-deadch", "title": "child", "project": "fno", "status": "ready",
         "parent": "ab-deadep", "plan_path": str(plan), "created_at": recent,
         "priority": "p2"},
    ])
    ids = [e["id"] for e in json.loads(_invoke("backlog", "ready", "--project", "fno").stdout)]
    assert "ab-live0" in ids
    assert "ab-stale0" not in ids      # stale-quarantined
    assert "ab-deadch" not in ids      # dead-ancestor

def test_reconcile_close_applies_the_ledger_rollup(tmp_graph, tmp_path, monkeypatch):
    """The MAINSTREAM close: a session lands its PR open, `done` exits 5 awaiting
    merge, and reconcile closes it at the merge. Without the rollup here,
    session_id / cost / points are never recorded on the normal path at all."""
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [{
        "plan_path": "recon.md", "cost_usd": 2.0, "points": 3,
        "sessions": ["sess-recon"], "completed": "2026-01-02T00:00:00Z",
    }]}) + "\n")
    import fno.graph._constants as gc
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    # Reconcile is the detached SessionStart sweep, so a live ambient session
    # here belongs to whoever started it, NOT to this node's work. The close
    # must attribute to the ledger, never leak this value onto the node.
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "ambient-reconcile-runner")

    entries = [
        {"id": "ab-recon001", "title": "Merged out of band", "status": "in_review",
         "project": "p", "domain": "code", "plan_path": "recon.md",
         "pr_number": 777, "pr_url": "https://github.com/o/r/pull/777",
         "blocked_by": []},
    ]
    _seed_graph_text(tmp_graph, json.dumps({"entries": entries}) + "\n")

    from fno.graph import _reconcile as rec
    monkeypatch.setattr(
        rec, "query_pr_merge_state",
        lambda n, **kw: rec.PrMergeState(
            number=777, state="MERGED",
            url="https://github.com/o/r/pull/777",
            merged_at="2026-01-02T00:00:00Z",
        ),
    )

    r = _invoke("backlog", "reconcile", "--node", "ab-recon001")
    assert r.exit_code == 0, r.stdout + r.stderr
    node = _by_id(tmp_graph)["ab-recon001"]
    assert node["completed_at"]
    assert node["session_id"] == "sess-recon"  # ledger, not "ambient-reconcile-runner"
    assert node["points"] == 3
    assert node["cost_usd"] == 2.0


def test_reconcile_rollup_preserves_an_existing_cost(tmp_graph, tmp_path, monkeypatch):
    """A prior cost stamp (fno backlog cost / a loop writer) timestamps its rows
    at recording time while the ledger row carries the completion time, so
    _apply_rollup would read the same run as distinct and double-count. Cost is
    fill-only here, matching cmd_done."""
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [{
        "plan_path": "recon.md", "cost_usd": 2.0, "points": 3,
        "sessions": ["sess-recon"], "completed": "2026-01-02T00:00:00Z",
    }]}) + "\n")
    import fno.graph._constants as gc
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)

    entries = [
        {"id": "ab-recon002", "title": "Pre-costed", "status": "in_review",
         "project": "p", "domain": "code", "plan_path": "recon.md",
         "pr_number": 778, "pr_url": "https://github.com/o/r/pull/778",
         "cost_usd": 9.99, "cost_sessions": [{"session_id": "pre", "cost_usd": 9.99}],
         "blocked_by": []},
    ]
    _seed_graph_text(tmp_graph, json.dumps({"entries": entries}) + "\n")

    from fno.graph import _reconcile as rec
    monkeypatch.setattr(
        rec, "query_pr_merge_state",
        lambda n, **kw: rec.PrMergeState(
            number=778, state="MERGED",
            url="https://github.com/o/r/pull/778", merged_at="2026-01-02T00:00:00Z",
        ),
    )

    r = _invoke("backlog", "reconcile", "--node", "ab-recon002")
    assert r.exit_code == 0, r.stdout + r.stderr
    node = _by_id(tmp_graph)["ab-recon002"]
    assert node["cost_usd"] == 9.99  # prior stamp preserved, not 11.99
    assert node["cost_sessions"] == [{"session_id": "pre", "cost_usd": 9.99}]
    assert node["points"] == 3  # non-cost rollup still applied


class _GetFakeTracker(_SnapshotFakeTracker):
    """Extends the snapshot fake: read() answers the open sentinels too."""

    def read(self, id):
        if id == "EXT-1":
            T, S = self._TrackerCandidate, self._TrackerState
            return T(id=id, title="Free work", state=S.open, blocked_by=["EXT-done"])
        if id == "EXT-2":
            T, S = self._TrackerCandidate, self._TrackerState
            return T(id=id, title="Waiting", state=S.open)
        return super().read(id)


def test_get_external_reads_tracker_and_sidecar_sentinels(
    tmp_graph, tmp_path, monkeypatch
):
    """AC2-HP (display path): `backlog get` under an external backend resolves
    the OPAQUE id exactly (no prefix-hex grammar), renders the five tracker
    fields plus sidecar sentinels, derives status at read time, and never
    returns the contradictory graph values."""
    _seed_graph_text(tmp_graph,
        json.dumps({"entries": [{
            "id": "EXT-1", "title": "graph-title-sentinel",
            "cwd": "/graph-cwd-sentinel", "pr_number": 999,
        }]}),
        encoding="utf-8",
    )
    sidecars = tmp_path / "sidecars"
    sidecars.mkdir()
    (sidecars / "EXT-1.json").write_text(
        json.dumps({"id": "EXT-1", "cwd": "/external-cwd", "pr_number": 7}),
        encoding="utf-8",
    )
    monkeypatch.setattr("fno.tracker.get_tracker", lambda *a, **k: _GetFakeTracker())
    monkeypatch.setattr("fno.paths.graph_json", lambda: tmp_graph)
    import fno.tracker.sidecar as sidecar_store

    monkeypatch.setattr(sidecar_store, "sidecar_path",
                        lambda i: sidecars / f"{i}.json")
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")

    r = _invoke("backlog", "get", "EXT-1")
    assert r.exit_code == 0, r.output
    doc = json.loads(r.output)
    assert doc["title"] == "Free work"  # tracker sentinel, not graph-title-sentinel
    assert doc["cwd"] == "/external-cwd"
    assert doc["pr_number"] == 7
    assert doc["state"] == "open"
    assert doc["status"] == "in_review"  # read-time derivation from pr evidence
    assert doc["_resolved_cwd"] == "/external-cwd"
    # Field mode reads through the same halves.
    r = _invoke("backlog", "get", "EXT-1", "--field", "cwd")
    assert r.exit_code == 0 and r.output.strip() == "/external-cwd"
    # A local-grammar spelling never resolves externally (dash-free so typer
    # does not read it as a flag).
    r = _invoke("backlog", "get", "1")
    assert r.exit_code == 1
    # EXT-2: open, no PR, no plan (no sidecar file written for it) - the same
    # three-way split selection filters on, not "any open node is ready".
    r = _invoke("backlog", "get", "EXT-2")
    assert r.exit_code == 0, r.output
    assert json.loads(r.output)["status"] == "idea"


def test_provenance_external_reads_sidecar_edges(
    tmp_graph, tmp_path, monkeypatch
):
    """AC2-HP (provenance path): `backlog provenance` under an external backend
    reads birth/spawn edges from the sidecar, joins the origin title from the
    tracker, and never reports the graph file's rows."""
    _seed_graph_text(tmp_graph,
        json.dumps({"entries": [{
            "id": "EXT-1", "source_session_id": "graph-sess",
            "sessions": [{"phase": "graph-only"}],
        }]}),
        encoding="utf-8",
    )
    sidecars = tmp_path / "sidecars"
    sidecars.mkdir()
    (sidecars / "EXT-1.json").write_text(
        json.dumps({"id": "EXT-1", "source_session_id": "ext-sess",
                    "source_harness": "claude",
                    "sessions": [{"phase": "do", "session_id": "ext-sess"}],
                    "source_node_id": "EXT-done"}),
        encoding="utf-8",
    )
    monkeypatch.setattr("fno.tracker.get_tracker", lambda *a, **k: _GetFakeTracker())
    monkeypatch.setattr("fno.paths.graph_json", lambda: tmp_graph)
    import fno.tracker.sidecar as sidecar_store

    monkeypatch.setattr(sidecar_store, "sidecar_path",
                        lambda i: sidecars / f"{i}.json")
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")

    r = _invoke("backlog", "provenance", "EXT-1", "--json")
    assert r.exit_code == 0, r.output
    doc = json.loads(r.output)
    assert doc["node_id"] == "EXT-1"
    assert doc["title"] == "Free work"
    assert doc["sessions"] == [{"phase": "do", "session_id": "ext-sess"}]
    assert doc["source_node_id"] == "EXT-done"
    assert doc["source_node_title"] == "Closed blocker"


def test_local_store_displays_refuse_cleanly_under_external(tmp_path, monkeypatch):
    """Display renders of the LOCAL store's full records (view) refuse with
    the backend named under an external selection - never a stale render."""
    absent = tmp_path / "absent.json"
    monkeypatch.setattr("fno.tracker.get_tracker", lambda *a, **k: _SnapshotFakeTracker())
    monkeypatch.setattr("fno.paths.graph_json", lambda: absent)
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")

    r = _invoke("backlog", "view")
    assert r.exit_code == 2, r.output
    assert "external" in r.output
