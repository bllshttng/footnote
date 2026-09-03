"""Routing + triage metric folds in `fno backlog triage health` (x-64cb US3).

Covers AC3-HP (routing metrics render with denominators), AC8-HP (validation
drop rate shows both numbers), and AC6-EDGE (event-gated: no fabricated zeros
when nothing recorded). The folds are pure dict->dict, so they are unit-tested
directly; one integration test pins the rendered wire.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.graph.triage import fold_routing_health, fold_triage_health

runner = CliRunner()


def _er(task, tier, resolved, warn=False):
    return {
        "ts": "2026-07-08T00:00:00Z",
        "type": "executor_resolved",
        "source": "target",
        "data": {"task": task, "resolved": resolved, "tier": tier, "warn_fallback": warn},
    }


def _ta(applied, proposed, dropped):
    return {
        "ts": "2026-07-08T00:00:00Z",
        "type": "triage_applied",
        "source": "backlog",
        "data": {"applied": applied, "proposed": proposed, "dropped": dropped},
    }


# --- AC6-EDGE: event-gated ---

def test_folds_return_none_when_no_events():
    assert fold_routing_health([]) is None
    assert fold_triage_health([]) is None
    # unrelated events do not trip the fold
    other = [{"type": "phase_transition", "data": {}}]
    assert fold_routing_health(other) is None
    assert fold_triage_health(other) is None


# --- AC3-HP: routing metrics with denominators + override proxy ---

def test_routing_fold_tiers_warn_and_override():
    events = [
        _er("1.1", "surface-inference", "impeccable"),
        _er("1.1", "task-block", "do"),  # overrides the inference (different value)
        _er("1.2", "default", "do", warn=True),
        _er("1.3", "surface-inference", "impeccable"),  # not overridden
    ]
    r = fold_routing_health(events)
    assert r["total"] == 4
    assert r["tier_distribution"] == {"surface-inference": 2, "task-block": 1, "default": 1}
    assert r["inference"] == 2
    assert r["warn_fallback_count"] == 1
    assert r["inferred_tasks"] == 2
    assert r["overridden_after_inference"] == 1


def test_routing_override_ignores_same_value_relock():
    # A later explicit lock resolving to the SAME value is not a mis-route.
    events = [
        _er("1.1", "surface-inference", "do"),
        _er("1.1", "plan-frontmatter", "do"),
    ]
    r = fold_routing_health(events)
    assert r["overridden_after_inference"] == 0


def test_routing_override_normalizes_legacy_do_alias():
    events = [
        _er("1.1", "surface-inference", "do"),
        _er("1.1", "plan-frontmatter", "tdd"),
    ]
    r = fold_routing_health(events)
    assert r["overridden_after_inference"] == 0


def test_routing_override_keyed_by_plan_not_bare_task_id():
    # Task ids are plan-relative: plan A's "1.1" inference must not be counted
    # overridden by plan B's "1.1" explicit lock (peer review, PR #285).
    a = _er("1.1", "surface-inference", "impeccable")
    a["data"]["plan_path"] = "A.md"
    b = _er("1.1", "task-block", "do")
    b["data"]["plan_path"] = "B.md"
    r = fold_routing_health([a, b])
    assert r["overridden_after_inference"] == 0
    assert r["inferred_tasks"] == 1  # only A's (plan,task) pair was inferred


# --- AC8-HP: validation drop rate shows both numbers ---

def test_triage_fold_drop_rate_denominator():
    events = [
        _ta({"priority_changes": 3, "dependencies": 0, "duplicates_flagged": 0, "deferred": 0}, 3, 0),
        _ta({"priority_changes": 1, "dependencies": 0, "duplicates_flagged": 0, "deferred": 0}, 2, 1),
    ]
    r = fold_triage_health(events)
    assert r["applies"] == 2
    assert r["applied_by_category"]["priority_changes"] == 4
    assert r["proposed"] == 5
    assert r["dropped"] == 1


# --- integration: rendered wire is event-gated + carries denominators ---

@pytest.fixture()
def health_env(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    import fno.graph.triage as triage

    for mod in (gc, gs):
        monkeypatch.setattr(mod, "GRAPH_JSON", g, raising=False)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md", raising=False)
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json", raising=False)
    ev = tmp_path / "events.jsonl"
    monkeypatch.setattr(triage, "_events_path", lambda: ev)
    return ev


def test_health_render_shows_routing_and_triage_with_denominators(health_env):
    ev = health_env
    lines = [
        _er("1.1", "surface-inference", "impeccable"),
        _er("1.1", "task-block", "do"),
        _er("1.2", "default", "do", warn=True),
        _ta({"priority_changes": 1, "dependencies": 0, "duplicates_flagged": 0, "deferred": 0}, 5, 1),
    ]
    ev.write_text("\n".join(json.dumps(x) for x in lines) + "\n")

    r = runner.invoke(app, ["backlog", "triage", "health"], catch_exceptions=False)
    assert r.exit_code == 0, r.output
    out = r.output
    assert "Executor routing (3 resolutions):" in out
    assert "surface-inference=1/3" in out
    assert "warn-fallback: 1/3" in out
    assert "override-after-inference (mis-route proxy): 1/1" in out
    assert "validation drop rate: 1/5" in out


def test_health_render_omits_sections_when_no_events(health_env):
    # AC6-EDGE: no fabricated zeros when nothing recorded.
    r = runner.invoke(app, ["backlog", "triage", "health"], catch_exceptions=False)
    assert r.exit_code == 0, r.output
    assert "Executor routing" not in r.output
    assert "Triage applies" not in r.output


# --- x-e451: supersession_unverified + blocked_by_held ---


def test_health_lists_unverified_supersession_rows(health_env, monkeypatch):
    """AC5-HP: a pending supersession whose successor merged with a PR but
    left declared surfaces uncovered breaches, naming both ends and the
    surfaces the evidence missed."""
    import fno.graph._constants as gc

    entries = [
        {
            "id": "ab-eb0e0001", "title": "old way", "status": "superseded",
            "superseded_by": "ab-9c240001",
            "supersession": {
                "successor": "ab-9c240001",
                "cause": "replaced by the settled lane",
                "surfaces": ["cli/src/a.py", "cli/src/b.py"],
                "matched_surfaces": ["cli/src/a.py"],
                "verified_at": None,
            },
        },
        {
            "id": "ab-9c240001", "title": "new way", "status": "done",
            "completed_at": "2026-08-30T00:00:00Z", "pr_number": 1307,
        },
    ]
    gc.GRAPH_JSON.write_text(json.dumps({"entries": entries}) + "\n")
    # The triage reader resolves through paths.graph_json(), not the module
    # constant, so the redirect must land there or the test reads the live
    # graph (which silently passes nothing and proves nothing).
    monkeypatch.setattr("fno.paths.graph_json", lambda: gc.GRAPH_JSON)

    r = runner.invoke(
        app, ["backlog", "triage", "health", "--check", "--json"],
        catch_exceptions=False,
    )
    assert r.exit_code == 4, r.output
    # --check prints the report JSON, then a one-line breach summary; parse
    # the document only.
    payload, _ = json.JSONDecoder().raw_decode(r.output)
    report = payload.get("report", payload)
    (row,) = report["supersession_unverified"]
    assert row["predecessor"] == "ab-eb0e0001"
    assert row["successor"] == "ab-9c240001"
    assert row["evidence_pr"] == 1307
    assert row["uncovered_surfaces"] == ["cli/src/b.py"]
    kinds = [b["key"] for b in payload["breaches"]]
    assert "supersession_unverified" in kinds


def test_health_lists_blocked_by_held_rows(health_env, monkeypatch):
    """AC4-EDGE: a deferred or missing blocker holds the dependent open, the
    row names both, and --check exits 4."""
    import fno.graph._constants as gc

    entries = [
        {
            "id": "ab-held0001", "title": "waiting", "status": "idea",
            "blocked_by": ["ab-defer001", "ab-ghost0001"],
        },
        {
            "id": "ab-defer001", "title": "parked", "status": "deferred",
            "deferred_at": "2026-08-01T00:00:00Z",
        },
    ]
    gc.GRAPH_JSON.write_text(json.dumps({"entries": entries}) + "\n")
    monkeypatch.setattr("fno.paths.graph_json", lambda: gc.GRAPH_JSON)

    r = runner.invoke(
        app, ["backlog", "triage", "health", "--check", "--json"],
        catch_exceptions=False,
    )
    assert r.exit_code == 4, r.output
    payload, _ = json.JSONDecoder().raw_decode(r.output)
    report = payload.get("report", payload)
    by_node = {row["node"]: row for row in report["blocked_by_held"]}
    assert by_node["ab-held0001"]["blocker_status"] == "deferred"
    assert by_node["ab-held0001"]["blocker"] == "ab-defer001"
    kinds = [b["key"] for b in payload["breaches"]]
    assert "blocked_by_held" in kinds
