"""Python-seam tests for the worked overlay: the fleet read, the
reachability pass, and the degradation contracts. The join itself (seat
records, crown exclusion, provenance, ship rows, closed receipts) is the
Rust `fno-agents worked-nodes` verb; its contracts are tested in Rust
(crates/fno-agents/src/worked_nodes.rs)."""
from __future__ import annotations

import pytest

from fno.claims.roster import RosterReading
from fno.graph.statuses import live_worked_node_ids


@pytest.fixture(autouse=True)
def _no_worked_reply(monkeypatch):
    """Hermetic overlay: the verb never runs unless a test provides a reply."""
    monkeypatch.setattr(
        "fno.graph.statuses._worked_nodes_reply", lambda rows: {}
    )


def _entry(node_id: str, *, status: str = "ready") -> dict:
    return {
        "id": node_id,
        "status": status,
        "sessions": [
            {
                "phase": "blueprint",
                "harness": "claude",
                "session_id": "session-1",
                "started_at": "2026-09-09T00:00:00Z",
            }
        ],
    }


def _reading(state: str, node: str = "ac1-node") -> RosterReading:
    row = {
        "name": "bp-worker",
        "state": state,
        "cwd": "/worktrees/ac1-node",
        "row_id": "session-1",
        "node": node,
    }
    return RosterReading(True, 1, {}, "", {"session-1": row}, 0, ())


def test_ac8_edge_degrades_loudly_when_roster_is_unreadable(monkeypatch, capsys):
    monkeypatch.setattr("fno.graph.store.read_graph_strict", lambda *_a, **_kw: [_entry("ac1-node")])
    monkeypatch.setattr(
        "fno.claims.roster.read_roster",
        lambda **_kw: RosterReading(False, 0, {}, "roster timeout"),
    )

    assert live_worked_node_ids() == {}
    assert "worked overlay degraded: roster timeout" in capsys.readouterr().err
    with pytest.raises(RuntimeError, match="roster timeout"):
        live_worked_node_ids(strict=True)




def test_graph_corruption_is_not_an_empty_worked_answer(monkeypatch, capsys):
    from fno.graph.store import GraphCorruptError

    monkeypatch.setattr(
        "fno.graph.store.read_graph_strict",
        lambda *_a, **_kw: (_ for _ in ()).throw(GraphCorruptError("graph corrupt")),
    )

    assert live_worked_node_ids() == {}
    assert "worked overlay degraded: graph corrupt" in capsys.readouterr().err
    with pytest.raises(GraphCorruptError, match="graph corrupt"):
        live_worked_node_ids(strict=True)




def test_an_injected_reading_is_the_one_used(monkeypatch):
    """The shared read is proven, not assumed: a caller that read
    the roster passes it in and the resolver never probes the fleet again.
    The claim reader joins through this parameter, so a second harness
    fan-out per status call would be the defect this pins shut."""
    def _boom(**_kw):
        raise AssertionError("roster re-probed despite an injected reading")

    monkeypatch.setattr("fno.claims.roster.read_roster", _boom)
    monkeypatch.setattr(
        "fno.graph.statuses._worked_nodes_reply",
        lambda rows: {"ac1-node": ["bp-worker"]},
    )

    assert live_worked_node_ids(
        strict=True, entries=[_entry("ac1-node")], reading=_reading("working")
    ) == {"ac1-node": ["bp-worker"]}




def test_all_terminal_graph_skips_the_roster_probe(monkeypatch):
    """No non-terminal node can be worked, so the fleet probe is wasted there
    and display paths keep their instant answer."""
    entries = [{"id": "done-1", "status": "done", "sessions": []}]

    def _boom(**_kw):
        raise AssertionError("roster probed on an all-terminal graph")

    monkeypatch.setattr("fno.graph.store.read_graph_strict", lambda *_a, **_kw: entries)
    monkeypatch.setattr("fno.claims.roster.read_roster", _boom)

    assert live_worked_node_ids(strict=True) == {}




def test_a_registry_only_fallback_answers_the_overlay(monkeypatch):
    """On a machine with no claude binary the promised registry-only fallback
    must actually answer the worked overlay, not refuse it (CI: undispatched
    went dark). Claim-status liveness keeps its refusal by default."""
    fallback = (
        "claude agents --json: claude binary not found on PATH; "
        "live_status unavailable, falling back to registry-only view"
    )
    monkeypatch.setattr(
        "fno.agents.watchdog.fleet_rows", lambda **_kw: ([], [fallback])
    )

    from fno.claims.roster import read_roster

    assert read_roster().consulted is False

    monkeypatch.setattr(
        "fno.graph.store.read_graph_strict",
        lambda *_a, **_kw: [_entry("x-6d3c")],
    )
    assert live_worked_node_ids(strict=True) == {}


def _crown_entry(node_id: str) -> dict:
    """The crown's own dispatch stamp: an open execute row naming the lead."""
    return {
        "id": node_id,
        "status": "in_progress",
        "sessions": [
            {
                "phase": "execute",
                "harness": "claude",
                "session_id": "crown-session",
                "started_at": "2026-10-04T22:27:02Z",
            }
        ],
    }




def test_read_roster_folds_unmeasurable_pairs(monkeypatch):
    """The producer's structured advisory line lands on the reading as node
    attribution, not as a blocking refusal."""
    monkeypatch.setattr(
        "fno.agents.watchdog.fleet_rows",
        lambda **_kw: ([], [
            "roster advisory: unmeasurable-row: "
            "harness=codex node=x-a238 name=bp-a238-king-brief",
        ]),
    )

    from fno.claims.roster import read_roster

    reading = read_roster()

    assert reading.consulted is True
    assert reading.unmeasurable_by_node == {"x-a238": ["bp-a238-king-brief"]}




def test_a_stopped_worker_drops_out_of_the_payload(monkeypatch):
    """Liveness stays a Python contract: the transcript/stopped-at predicate
    filters the payload, so a killed worker never reaches the verb."""
    captured: list[list[dict]] = []

    def fake_reply(rows):
        captured.append(rows)
        return {"ac1-node": ["bp-worker"]} if rows else {}

    monkeypatch.setattr("fno.graph.statuses._worked_nodes_reply", fake_reply)
    state = ["working"]
    monkeypatch.setattr(
        "fno.graph.store.read_graph_strict", lambda *_a, **_kw: [_entry("ac1-node")]
    )
    monkeypatch.setattr(
        "fno.claims.roster.read_roster", lambda **_kw: _reading(state[0])
    )

    assert live_worked_node_ids() == {"ac1-node": ["bp-worker"]}
    state[0] = "killed"
    assert live_worked_node_ids() == {}
    # The killed worker never reaches the verb: its second payload is empty,
    # and the verb's empty-rows answer reads as no worked nodes.
    assert len(captured) == 2
    assert [r["name"] for r in captured[0]] == ["bp-worker"]
    assert captured[1] == []


def test_the_payload_passes_attribution_and_marks_unmeasured(monkeypatch):
    """The delegate's one Python-side contract: the rows it sends the verb
    carry each row's node attribution (attributed rows survive the dedupe a
    seat-only pass would lose) and an unmeasured row rides its marked label."""
    captured: list[list[dict]] = []

    def fake_reply(rows):
        captured.append(rows)
        return {"x-node": ["t-worker"]}

    monkeypatch.setattr("fno.graph.statuses._worked_nodes_reply", fake_reply)
    entries = [{"id": "x-node", "status": "in_progress", "sessions": []}]
    reading = RosterReading(
        True, 2,
        {"x-node": [{"name": "t-worker", "state": "working",
                     "cwd": "/worktrees/x-node", "row_id": "s-1"}]},
        "",
        {"s-2": {"name": "t-drifter", "state": "working",
                 "cwd": "/elsewhere", "row_id": "s-2"}},
        0, (),
        {"x-node": ("bp-nosid",)},
    )
    assert live_worked_node_ids(strict=True, entries=entries, reading=reading) == {
        "x-node": ["t-worker"]
    }

    rows = captured[0]
    by_name = {r["name"]: r for r in rows}
    assert by_name["t-worker"]["node"] == "x-node"
    assert by_name["t-worker"]["session"] == "s-1"
    assert by_name["t-drifter"]["node"] is None
    assert "unmeasurable" in by_name["bp-nosid"]["label"]
    assert by_name["bp-nosid"]["session"] == ""
