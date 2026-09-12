"""``fno agents court``: the half of the stuck read that only Python can answer.

The verdict itself is computed in ``court_fold.rs``, beside the rows it judges,
and its rules are covered there. What stays here is what the native fold cannot
see: the spawn gate's verdict, a session id judged against the registry, and the
wiring that folds for every render shape while keeping the bare ``--json``
contract its callers pin.

Every read here is a POSITIVE marker read (AGENTS.md). A court that prints
``stuck: nothing`` because its fold never ran must not render the same as a
court that looked and found nothing.
"""
from __future__ import annotations

import json
from pathlib import Path

from .test_crown_court import _entry, _prepare


def _king(name: str = "king-a", scope: str = "alpha"):
    return _entry(name, status="busy", crown_level=1, crown_scope=scope, crown_grantor="human")


def _node(nid: str, **kw) -> dict:
    row = {
        "id": nid,
        "slug": f"{nid}-slug",
        "status": "ready",
        "worker": None,
        "claim_state": "no-record",
        "claim_basis": None,
        "pr_number": None,
        "sessions": [],
        "age_hours": 0.1,
        "blocked_by": None,
        "blocked_reason": None,
    }
    row.update(kw)
    return row


def _fold(monkeypatch, nodes: list[dict], stuck: dict | None = None, line: str = "") -> None:
    """Inject what the native fold would have returned.

    The fold is a subprocess to the native binary. These tests are about what
    the Python does with its answer, so the answer is supplied directly.
    """
    from fno.agents import court

    def fake_fold(crowns):
        for crown in crowns:
            crown["scope_nodes"] = {
                "status": "ok",
                "total": len(nodes),
                "counts": {},
                "nodes": [dict(n) for n in nodes],
                "omitted": 0,
            }
        return {
            "stuck": stuck
            if stuck is not None
            else {
                "unclaimed": [],
                "blocked": [],
                "unproven_claim": [],
                "in_review": [],
                "blind": [],
                "threshold_minutes": 60,
            },
            "stuck_line": line,
        }

    monkeypatch.setattr(court, "fold_scope_nodes", fake_fold)


def _gate(monkeypatch, payload) -> None:
    from fno.agents import spawn_gate

    monkeypatch.setattr(spawn_gate, "probe_capacity", lambda: payload)


def _accepted(monkeypatch) -> None:
    _gate(monkeypatch, {"verdict": "accepted", "lanes": {"zai": {"cap": 4, "live": 1}}})


def test_a_full_king_share_is_named_with_its_numbers(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.court import render_court

    _prepare(monkeypatch, tmp_path, [_king()], graph_entries=[])
    _fold(monkeypatch, [])
    _gate(
        monkeypatch,
        {
            "verdict": "refused",
            "reason": "king_share",
            "message": "this reign holds 7 of max_live 35",
            "held": 7,
            "share": 7,
            "kings": 5,
        },
    )

    court = json.loads(render_court(as_json=True))
    table = render_court(as_json=False)

    assert court["gate"]["verdict"] == "refused"
    assert court["gate"]["reason"] == "king_share"
    assert (court["gate"]["held"], court["gate"]["share"], court["gate"]["kings"]) == (7, 7, 5)
    # A refused gate is why a ready node will not dispatch, so it reaches the line.
    assert "gate refused king_share" in table


def test_a_gate_that_cannot_be_read_answers_unknown_and_the_court_still_renders(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents import spawn_gate
    from fno.agents.court import render_court

    _prepare(monkeypatch, tmp_path, [_king()], graph_entries=[])
    _fold(monkeypatch, [])

    def boom():
        raise RuntimeError("the probe fell over")

    monkeypatch.setattr(spawn_gate, "probe_capacity", boom)

    court = json.loads(render_court(as_json=True))
    table = render_court(as_json=False)

    assert court["gate"]["verdict"] == "unknown"
    assert "the probe fell over" in court["gate"]["reason"]
    # An unknown gate is a blind spot, so the line must not read `nothing`.
    line = next(ln for ln in table.splitlines() if ln.startswith("stuck:"))
    assert "the spawn gate answered unknown" in line
    assert line != "stuck: nothing"
    # The rest of the payload is unharmed by a gate that could not answer.
    assert court["summary"]["total"] == 1
    assert court["crowns"][0]["scope"] == "alpha"


def test_a_session_the_registry_does_not_carry_reads_null_never_false(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents.court import render_court

    _prepare(monkeypatch, tmp_path, [_king()], graph_entries=[])
    _fold(monkeypatch, [_node("x-1", sessions=["king-a-session", "ghost-session"])])
    _accepted(monkeypatch)

    court = json.loads(render_court(as_json=True, nodes=True))

    rows = court["crowns"][0]["scope_nodes"]["nodes"][0]["sessions"]
    by_id = {r["id"]: r for r in rows}
    assert by_id["king-a-session"]["live"] is True
    assert by_id["king-a-session"]["status"] == "busy"
    # Absent from the registry is not the same answer as a terminal row.
    assert by_id["ghost-session"]["live"] is None
    assert by_id["ghost-session"]["status"] is None


def test_the_fold_verdict_reaches_the_table_and_the_json(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.court import render_court

    _prepare(monkeypatch, tmp_path, [_king()], graph_entries=[])
    _fold(
        monkeypatch,
        [_node("x-1", age_hours=2.0)],
        stuck={
            "unclaimed": ["x-1", "x-2"],
            "blocked": [],
            "unproven_claim": [],
            "in_review": [],
            "blind": [],
            "threshold_minutes": 60,
        },
        line="2 ready over 60m with no worker (x-1, x-2)",
    )
    _accepted(monkeypatch)

    table = render_court(as_json=False)
    court = json.loads(render_court(as_json=True))

    line = [ln for ln in table.splitlines() if ln.startswith("stuck:")]
    assert len(line) == 1
    assert "x-1" in line[0] and "x-2" in line[0]
    # A machine reads the same verdict a person does.
    assert court["summary"]["stuck"]["unclaimed"] == ["x-1", "x-2"]


def test_a_quiet_fleet_reads_nothing(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.court import render_court

    _prepare(monkeypatch, tmp_path, [_king()], graph_entries=[])
    _fold(monkeypatch, [_node("x-1", claim_state="live", worker="worker-a")])
    _accepted(monkeypatch)

    assert "stuck: nothing" in render_court(as_json=False)


def test_a_fold_that_answered_nothing_says_so_rather_than_nothing(
    tmp_path: Path, monkeypatch
) -> None:
    import subprocess

    from fno.agents.court import render_court

    _prepare(monkeypatch, tmp_path, [_king()], graph_entries=[])
    _accepted(monkeypatch)

    def timeout(*a, **kw):
        raise subprocess.TimeoutExpired(
            cmd=["fno-agents", "court-fold", "--graph", "/g"], timeout=30
        )

    # court.py imports subprocess inside the function, so the stdlib module is
    # the only handle a test has on it.
    monkeypatch.setattr(subprocess, "run", timeout)

    table = render_court(as_json=False)

    line = next(ln for ln in table.splitlines() if ln.startswith("stuck:"))
    assert "the scope fold did not run" in line
    assert line != "stuck: nothing"
    # A TimeoutExpired stringifies to its whole argv, which would bury the fault.
    assert "--graph" not in line


def test_a_binary_older_than_this_court_says_so_not_that_the_fold_failed(
    tmp_path: Path, monkeypatch
) -> None:
    from fno.agents import court
    from fno.agents.court import render_court

    _prepare(monkeypatch, tmp_path, [_king()], graph_entries=[])
    _accepted(monkeypatch)

    def old_binary(crowns):
        for crown in crowns:
            crown["scope_nodes"] = {
                "status": "ok", "total": 0, "counts": {}, "nodes": [], "omitted": 0,
            }
        # The pre-2026-09-12 payload: rows, and no stuck verdict.
        return {"scope_nodes": {}}

    monkeypatch.setattr(court, "fold_scope_nodes", old_binary)

    line = next(
        ln for ln in render_court(as_json=False).splitlines() if ln.startswith("stuck:")
    )

    # A fold that answered and a fold that never ran are different faults, and
    # reading the first as the second is a false blind.
    assert "predates this court" in line
    assert "the scope fold did not run" not in line


def test_a_bare_json_render_carries_no_scope_nodes(tmp_path: Path, monkeypatch) -> None:
    from fno.agents.court import render_court

    _prepare(monkeypatch, tmp_path, [_king()], graph_entries=[])
    _fold(monkeypatch, [_node("x-1")])
    _accepted(monkeypatch)

    plain = json.loads(render_court(as_json=True))
    folded = json.loads(render_court(as_json=True, nodes=True))

    assert "scope_nodes" not in plain["crowns"][0]
    assert "scope_nodes" in folded["crowns"][0]
    # The stuck verdict is computed either way; only the rows are dropped.
    assert plain["summary"]["stuck"] == folded["summary"]["stuck"]
