"""Tests for first-class defer/undefer on backlog nodes.

Covers:
- ``deferred_at`` / ``deferred_reason`` schema and derivation cascade
- ``fno backlog defer`` direct verb (with required ``--reason``)
- ``fno backlog undefer`` reversal verb
- ``--include-deferred`` flag on ``ready`` / ``next``
- ``status`` summary surfaces a ``deferred`` count
- ``triage`` proposal action (validate + apply)
- Legacy ``completed_at: "deferred:<ts>"`` rows migrate to the new schema
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    """Fresh empty graph.json routed to tmp_path."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    return g


def _invoke(*args, input=None):
    return runner.invoke(app, list(args), input=input, catch_exceptions=False)


def _read_entries(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


def _seed_with_plan(tmp_path, title: str = "Plan") -> str:
    plan = tmp_path / f"{title.lower().replace(' ', '-')}.md"
    plan.write_text(f"---\ntitle: {title}\n---\n# Body\n")
    r = _invoke("backlog", "intake", str(plan))
    assert r.exit_code == 0, r.output
    entries = json.loads(open(plan.parent.parent / "graph.json").read_text())["entries"] \
        if (plan.parent.parent / "graph.json").exists() else []
    if not entries:
        # Resolve via the runner's graph_path (tmp_graph fixture sets it).
        from fno.graph._constants import GRAPH_JSON
        entries = json.loads(Path(GRAPH_JSON).read_text())["entries"]
    return next(e["id"] for e in entries if e.get("plan_path") == str(plan))


# ---------------------------------------------------------------------------
# Status derivation cascade
# ---------------------------------------------------------------------------


def test_status_deferred_derived_from_field(tmp_graph, tmp_path):
    """A node with ``deferred_at`` set derives to ``status: deferred``."""
    node_id = _seed_with_plan(tmp_path, "Plan A")

    r = _invoke("backlog", "defer", node_id, "--reason", "stale")
    assert r.exit_code == 0, r.output

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("deferred_at"), "deferred_at should be set to an ISO timestamp"
    assert node.get("deferred_reason") == "stale"
    assert node.get("status") == "deferred", (
        f"expected derived deferred status; got {node.get('status')!r}"
    )


def test_deferred_overrides_blocked(tmp_graph, tmp_path):
    """Deferred wins over blocked-by an unresolved blocker."""
    a = _invoke("--json", "backlog", "add", "Blocker A")
    blocker_id = json.loads(a.stdout)["id"]

    plan = tmp_path / "blocked-plan.md"
    plan.write_text("---\ntitle: Blocked\n---\n# Body\n")
    _invoke("backlog", "intake", str(plan), "--deps", blocker_id)
    entries = _read_entries(tmp_graph)
    target_id = next(e["id"] for e in entries if e.get("plan_path") == str(plan))

    r = _invoke("backlog", "defer", target_id, "--reason", "wait on Q3")
    assert r.exit_code == 0, r.output

    entries = _read_entries(tmp_graph)
    target = next(e for e in entries if e["id"] == target_id)
    assert target.get("status") == "deferred", (
        f"deferred should beat blocked; got {target.get('status')!r}"
    )


def test_deferred_does_not_override_done(tmp_graph, tmp_path):
    """Done wins over deferred. A completed node stays done."""
    node_id = _seed_with_plan(tmp_path, "Plan Done")
    _invoke("backlog", "done", node_id, "--skip-stamp")

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("status") == "done"

    # Force-set deferred_at via direct mutation; recompute should still pick done.
    import fno.graph._constants as gc
    data = json.loads(gc.GRAPH_JSON.read_text())
    for e in data["entries"]:
        if e["id"] == node_id:
            e["deferred_at"] = "2026-04-30T00:00:00+00:00"
            e["deferred_reason"] = "should not surface"
    gc.GRAPH_JSON.write_text(json.dumps(data))

    # Trigger recompute via any mutation
    _invoke("backlog", "add", "trigger")
    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("status") == "done", (
        f"done must beat deferred; got {node.get('status')!r}"
    )


def test_legacy_deferred_completed_at_migrates(tmp_graph):
    """Pre-feature rows with ``completed_at: "deferred:<ts>"`` flip to the new schema."""
    legacy_ts = "2026-04-01T12:00:00+00:00"
    tmp_graph.write_text(json.dumps({
        "entries": [
            {
                "id": "ab-legacy42",
                "title": "Legacy deferred row",
                "type": "feature",
                "priority": "p2",
                "domain": "code",
                "blocked_by": [],
                "session_id": None,
                "claimed_at": None,
                "completed_at": f"deferred:{legacy_ts}",
                "plan_path": "plan.md",
                "created_at": "2026-04-01T00:00:00+00:00",
            }
        ]
    }))

    # Trigger recompute via any mutation.
    _invoke("backlog", "add", "trigger migration")

    entries = _read_entries(tmp_graph)
    legacy = next(e for e in entries if e["id"] == "ab-legacy42")
    assert legacy.get("completed_at") in (None, ""), (
        f"legacy completed_at prefix should be cleared; got {legacy.get('completed_at')!r}"
    )
    assert legacy.get("deferred_at") == legacy_ts, (
        f"deferred_at should be migrated from the prefix; got {legacy.get('deferred_at')!r}"
    )
    assert legacy.get("status") == "deferred"


# ---------------------------------------------------------------------------
# Direct verbs: defer + undefer
# ---------------------------------------------------------------------------


def test_defer_command_sets_deferred_at_and_reason(tmp_graph, tmp_path):
    """``backlog defer ID --reason X`` sets both fields and emits an ack."""
    node_id = _seed_with_plan(tmp_path, "Plan B")

    r = _invoke("backlog", "defer", node_id, "--reason", "Waiting on Q3 budget approval")
    assert r.exit_code == 0, r.output
    assert "deferred" in r.output.lower()

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("deferred_at") and "T" in node["deferred_at"], (
        f"deferred_at should be ISO timestamp; got {node.get('deferred_at')!r}"
    )
    assert node.get("deferred_reason") == "Waiting on Q3 budget approval"


def test_defer_command_requires_reason(tmp_graph, tmp_path):
    """``backlog defer ID`` without --reason exits non-zero."""
    node_id = _seed_with_plan(tmp_path, "Plan Need Reason")

    r = runner.invoke(app, ["backlog", "defer", node_id], catch_exceptions=True)
    assert r.exit_code != 0, "defer without --reason should fail"


def test_undefer_command_clears_state(tmp_graph, tmp_path):
    """``backlog undefer ID`` clears deferred_at and deferred_reason."""
    node_id = _seed_with_plan(tmp_path, "Plan C")
    _invoke("backlog", "defer", node_id, "--reason", "stale")

    r = _invoke("backlog", "undefer", node_id)
    assert r.exit_code == 0, r.output

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert not node.get("deferred_at"), (
        f"deferred_at should be cleared; got {node.get('deferred_at')!r}"
    )
    assert not node.get("deferred_reason")
    assert node.get("status") == "ready", (
        f"undefer should restore ready; got {node.get('status')!r}"
    )


def test_undefer_warns_when_not_deferred(tmp_graph, tmp_path):
    """``backlog undefer ID`` on a non-deferred node prints a warning, exits 0."""
    node_id = _seed_with_plan(tmp_path, "Plan D")

    r = _invoke("backlog", "undefer", node_id)
    assert r.exit_code == 0, r.output
    combined = (r.stdout or "") + (r.stderr or "")
    assert "warn" in combined.lower() or "not deferred" in combined.lower(), (
        f"expected a warning; got: {combined}"
    )


def test_defer_after_done_transitions_to_deferred(tmp_graph, tmp_path):
    """Deferring an already-done node clears completed_at so the cascade flips to deferred.

    Regression for Gemini external-review finding: without clearing
    completed_at, the `done > deferred` precedence in recompute_statuses
    would silently keep the row pinned to done.
    """
    node_id = _seed_with_plan(tmp_path, "Plan Done Then Defer")
    _invoke("backlog", "done", node_id, "--skip-stamp")

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("status") == "done"
    assert node.get("completed_at")

    r = _invoke("backlog", "defer", node_id, "--reason", "reopened, parking it")
    assert r.exit_code == 0, r.output

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("completed_at") in (None, ""), (
        f"completed_at must be cleared when deferring; got {node.get('completed_at')!r}"
    )
    assert node.get("deferred_at"), "deferred_at must be set"
    assert node.get("status") == "deferred", (
        f"cascade must flip to deferred after defer; got {node.get('status')!r}"
    )


def test_triage_defer_after_done_transitions_to_deferred(tmp_graph, tmp_path):
    """Triage apply lands the same done -> deferred transition cleanly."""
    node_id = _seed_with_plan(tmp_path, "Plan Triage Done Then Defer")
    _invoke("backlog", "done", node_id, "--skip-stamp")

    proposal = tmp_path / "p.json"
    proposal.write_text(json.dumps({
        "defer": [{"id": node_id, "reason": "needs revisit"}],
    }))
    r = _invoke("backlog", "triage", "apply", str(proposal))
    assert r.exit_code == 0, r.output

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("completed_at") in (None, "")
    assert node.get("status") == "deferred"


def test_defer_rejects_blank_reason(tmp_graph, tmp_path):
    """``backlog defer ID --reason "   "`` is rejected at the CLI boundary.

    Mirrors the triage validator which drops entries with a blank reason.
    Without this guard the two write paths diverge: the direct CLI verb
    would accept blank, the triage proposal would reject it, leaving graph
    state shape-dependent on which entry point produced it.
    """
    node_id = _seed_with_plan(tmp_path, "Plan Blank Reason")

    r = runner.invoke(
        app,
        ["backlog", "defer", node_id, "--reason", "   "],
        catch_exceptions=True,
    )
    assert r.exit_code != 0, "defer with whitespace-only reason must fail"

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert not node.get("deferred_at"), "no defer state should land on rejection"


# ---------------------------------------------------------------------------
# Filters: --include-deferred on ready / next
# ---------------------------------------------------------------------------


def test_deferred_excluded_from_ready_default(tmp_graph, tmp_path):
    """``backlog ready`` omits deferred nodes."""
    node_id = _seed_with_plan(tmp_path, "Plan E")
    _invoke("backlog", "defer", node_id, "--reason", "stale")

    r = _invoke("backlog", "ready", "--all")
    assert r.exit_code == 0, r.output
    listing = json.loads(r.stdout)
    ids = [e["id"] for e in listing]
    assert node_id not in ids, "deferred should not appear in default `ready` listing"


def test_deferred_included_with_flag(tmp_graph, tmp_path):
    """``backlog ready --include-deferred`` surfaces deferred rows."""
    node_id = _seed_with_plan(tmp_path, "Plan F")
    _invoke("backlog", "defer", node_id, "--reason", "stale")

    r = _invoke("backlog", "ready", "--all", "--include-deferred")
    assert r.exit_code == 0, r.output
    listing = json.loads(r.stdout)
    ids = [e["id"] for e in listing]
    assert node_id in ids, "deferred should appear when --include-deferred is set"


def test_deferred_excluded_from_next_default(tmp_graph, tmp_path):
    """``backlog next`` skips deferred nodes."""
    plan_a = tmp_path / "plan-a.md"
    plan_a.write_text("---\ntitle: A\n---\n# A\n")
    _invoke("backlog", "intake", str(plan_a), "--priority", "p1")

    plan_b = tmp_path / "plan-b.md"
    plan_b.write_text("---\ntitle: B\n---\n# B\n")
    _invoke("backlog", "intake", str(plan_b), "--priority", "p2")

    entries = _read_entries(tmp_graph)
    a_id = next(e["id"] for e in entries if e.get("plan_path") == str(plan_a))
    _invoke("backlog", "defer", a_id, "--reason", "stale p1")

    r = _invoke("backlog", "next", "--all")
    assert r.exit_code == 0, r.output
    payload = json.loads(r.stdout)
    assert payload is not None
    assert payload["id"] != a_id, "deferred p1 should not be picked over ready p2"


# ---------------------------------------------------------------------------
# Status summary deferred count
# ---------------------------------------------------------------------------


def test_status_summary_shows_deferred_count(tmp_graph, tmp_path):
    """``backlog status`` prints a deferred count line when nonzero."""
    node_id = _seed_with_plan(tmp_path, "Plan G")
    _invoke("backlog", "defer", node_id, "--reason", "stale")

    r = _invoke("backlog", "status", "--all")
    assert r.exit_code == 0, r.output
    assert "deferred" in r.output.lower(), (
        f"status output should mention deferred; got:\n{r.output}"
    )


# ---------------------------------------------------------------------------
# Triage: defer proposal action
# ---------------------------------------------------------------------------


def test_triage_defer_proposal_validates_and_applies(tmp_graph, tmp_path):
    """A proposal with a defer entry validates clean and applies the defer."""
    node_id = _seed_with_plan(tmp_path, "Plan Triage Defer")

    proposal = tmp_path / "proposal.json"
    proposal.write_text(json.dumps({
        "dependencies": [],
        "priority_changes": [],
        "duplicates": [],
        "defer": [{"id": node_id, "reason": "out of season"}],
    }))

    r = _invoke("backlog", "triage", "validate", str(proposal))
    assert r.exit_code == 0, f"validate should succeed; got:\n{r.output}"
    cleaned = json.loads(r.stdout)
    assert cleaned.get("defer"), "cleaned proposal should preserve defer"
    assert cleaned["defer"][0]["id"] == node_id

    r = _invoke("backlog", "triage", "apply", str(proposal))
    assert r.exit_code == 0, r.output
    applied = json.loads(r.stdout).get("applied", {})
    assert applied.get("deferred") == 1, f"expected applied.deferred==1; got {applied}"

    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("status") == "deferred"
    assert node.get("deferred_reason") == "out of season"


def test_triage_defer_drops_unknown_id_and_missing_reason(tmp_graph, tmp_path):
    """Invalid defer entries (unknown id, blank reason) are dropped with errors."""
    node_id = _seed_with_plan(tmp_path, "Plan Triage Bad")

    proposal = tmp_path / "proposal.json"
    proposal.write_text(json.dumps({
        "defer": [
            {"id": "ab-unknown1", "reason": "not a real id"},
            {"id": node_id, "reason": ""},
        ],
    }))

    r = runner.invoke(app, ["backlog", "triage", "validate", str(proposal)],
                      catch_exceptions=True)
    # Validation reports errors with exit code 3 (drops bad edges).
    assert r.exit_code != 0
    cleaned = json.loads(r.stdout)
    assert cleaned.get("defer", []) == [], "all bad entries should be dropped"


def test_triage_propose_skeleton_includes_defer(tmp_graph, tmp_path):
    """``triage propose`` emits a skeleton with a defer key for LLMs to fill."""
    _seed_with_plan(tmp_path, "Plan Skeleton")

    r = _invoke("backlog", "triage", "propose", "--all")
    assert r.exit_code == 0, r.output
    skeleton = json.loads(r.stdout)
    assert "defer" in skeleton, (
        f"propose skeleton should expose `defer`; got keys {list(skeleton.keys())}"
    )


def test_triage_apply_exits_nonzero_on_drops(tmp_graph, tmp_path):
    """``triage apply`` exits 3 when defer entries are dropped.

    Symmetry with ``triage validate``: a scripted caller piping `apply`
    must be able to detect partial application via exit code, not just
    by parsing the JSON ``dropped_due_to_validation`` count.
    """
    node_id = _seed_with_plan(tmp_path, "Plan Apply Drops")
    proposal = tmp_path / "bad.json"
    proposal.write_text(json.dumps({
        "defer": [
            {"id": "ab-unknown1", "reason": "bad id"},
            {"id": node_id, "reason": "good"},
        ],
    }))

    r = runner.invoke(
        app,
        ["backlog", "triage", "apply", str(proposal)],
        catch_exceptions=True,
    )
    assert r.exit_code == 3, (
        f"apply with a dropped entry should exit 3; got {r.exit_code} output={r.output}"
    )
    # The good entry still lands - apply is best-effort partial.
    entries = _read_entries(tmp_graph)
    node = next(e for e in entries if e["id"] == node_id)
    assert node.get("status") == "deferred"
