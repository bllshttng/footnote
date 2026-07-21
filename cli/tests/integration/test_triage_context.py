"""Tests for triage context payload enrichment.

These tests pin the shape of ``_candidate_record`` so the LLM reasoning
prompt gets a stable contract to pattern-match on. The function is the
canonical "what does the LLM see?" surface for triage; future enrichments
extend it without breaking existing keys.

The unit tests call ``_candidate_record`` directly (it is a pure dict->dict
mapping). The CLI integration tests at the bottom of this file run the
full ``fno backlog triage context`` wire so a future refactor that
decouples ``_candidate_record`` from ``cmd_context`` can't silently drop
fields the LLM reasoning prompt depends on.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.graph.triage import _candidate_record


runner = CliRunner()


def test_candidate_record_includes_size():
    entry = {"id": "ab-1", "title": "X", "size": "M"}
    record = _candidate_record(entry, deep=False)
    assert record["size"] == "M"


def test_candidate_record_size_null_when_unset():
    entry = {"id": "ab-1", "title": "X"}
    record = _candidate_record(entry, deep=False)
    assert record["size"] is None


def test_candidate_record_includes_details():
    entry = {"id": "ab-1", "title": "X", "details": "Salesforce-Aura scrape"}
    record = _candidate_record(entry, deep=False)
    assert record["details"] == "Salesforce-Aura scrape"


def test_candidate_record_includes_domain():
    entry = {"id": "ab-1", "title": "X", "domain": "code"}
    record = _candidate_record(entry, deep=False)
    assert record["domain"] == "code"


def test_claim_history_aggregates_cost():
    entry = {
        "id": "ab-1",
        "title": "X",
        "cost_sessions": [{"cost_usd": 1.5}, {"cost_usd": 2.0}],
    }
    record = _candidate_record(entry, deep=False)
    assert record["claim_history"]["total_cost_usd"] == 3.5


def test_claim_history_session_count():
    entry = {
        "id": "ab-1",
        "title": "X",
        "cost_sessions": [{"cost_usd": 1.5}, {"cost_usd": 2.0}],
    }
    record = _candidate_record(entry, deep=False)
    assert record["claim_history"]["session_count"] == 2


def test_claim_history_handles_missing_cost_sessions():
    entry = {"id": "ab-1", "title": "X"}
    record = _candidate_record(entry, deep=False)
    assert record["claim_history"]["session_count"] == 0
    assert record["claim_history"]["total_cost_usd"] == 0
    assert record["claim_history"]["last_claimed_at"] is None


def test_claim_history_passes_through_claimed_at():
    entry = {
        "id": "ab-1",
        "title": "X",
        "claimed_at": "2026-04-27T10:00:00Z",
    }
    record = _candidate_record(entry, deep=False)
    assert record["claim_history"]["last_claimed_at"] == "2026-04-27T10:00:00Z"


def test_claim_history_skips_malformed_sessions():
    """Malformed sessions must not crash AND must not inflate session_count.

    `session_count` reports prior claim attempts the LLM should reason
    about; entries with no usable cost (non-dict, missing cost_usd,
    non-numeric cost_usd) are not real claim attempts and must be
    filtered. This keeps session_count and total_cost_usd's denominator
    aligned so the LLM never sees "4 sessions, $3 total" implying a
    free session that never existed.
    """
    entry = {
        "id": "ab-1",
        "title": "X",
        # Mix of dict (valid) and non-dict (should not crash the sum).
        "cost_sessions": [{"cost_usd": 1.0}, "not-a-dict", None, {"cost_usd": 2.0}],
    }
    record = _candidate_record(entry, deep=False)
    assert record["claim_history"]["session_count"] == 2
    assert record["claim_history"]["total_cost_usd"] == 3.0


def test_claim_history_handles_non_list_cost_sessions():
    """Legacy schemas may have a dict-shaped cost_sessions; tolerate it."""
    entry = {
        "id": "ab-1",
        "title": "X",
        "cost_sessions": {"sess-1": {"cost_usd": 5.0}},
    }
    record = _candidate_record(entry, deep=False)
    assert record["claim_history"]["session_count"] == 0
    assert record["claim_history"]["total_cost_usd"] == 0


def test_claim_history_skips_string_cost_usd():
    """Non-numeric cost_usd would crash sum() with TypeError; filter it."""
    entry = {
        "id": "ab-1",
        "title": "X",
        "cost_sessions": [
            {"cost_usd": "0.42"},  # legacy string-typed cost
            {"cost_usd": None},
            {"cost_usd": 1.5},
        ],
    }
    record = _candidate_record(entry, deep=False)
    assert record["claim_history"]["session_count"] == 1
    assert record["claim_history"]["total_cost_usd"] == 1.5


def test_claim_history_skips_bool_cost_usd():
    """Booleans are int-subclass; `True` would silently sum to 1."""
    entry = {
        "id": "ab-1",
        "title": "X",
        "cost_sessions": [{"cost_usd": True}, {"cost_usd": 2.0}],
    }
    record = _candidate_record(entry, deep=False)
    assert record["claim_history"]["session_count"] == 1
    assert record["claim_history"]["total_cost_usd"] == 2.0


def test_ship_state_passes_through_pr_number():
    entry = {"id": "ab-1", "title": "X", "pr_number": 42, "merge_status": "merged"}
    record = _candidate_record(entry, deep=False)
    assert record["ship_state"]["pr_number"] == 42
    assert record["ship_state"]["merge_status"] == "merged"


def test_ship_state_null_when_unset():
    entry = {"id": "ab-1", "title": "X"}
    record = _candidate_record(entry, deep=False)
    assert record["ship_state"]["pr_number"] is None
    assert record["ship_state"]["merge_status"] is None


def test_existing_fields_still_present():
    """Regression: enrichment is purely additive, original keys unchanged."""
    entry = {
        "id": "ab-1",
        "title": "X",
        "priority": "p1",
        "blocked_by": ["ab-0"],
        "plan_path": "plans/foo.md",
        "roadmap_id": "rm-1",
        "created_at": "2026-04-27T10:00:00Z",
        "source": "intake",
        "status": "ready",
    }
    record = _candidate_record(entry, deep=False)
    for key in (
        "id",
        "title",
        "priority",
        "blocked_by",
        "plan_path",
        "roadmap_id",
        "created_at",
        "source",
        "status",
    ):
        assert key in record, f"missing key {key!r}"
    assert record["status"] == "ready"
    assert record["blocked_by"] == ["ab-0"]


def test_deep_mode_still_includes_plan_excerpt(tmp_path):
    """Regression: deep-mode plan_excerpt still works alongside new fields."""
    plan = tmp_path / "spec.md"
    plan.write_text("# Spec\n\nbody line\n")
    entry = {
        "id": "ab-1",
        "title": "X",
        "plan_path": str(plan),
        "size": "S",
    }
    record = _candidate_record(entry, deep=True)
    assert "plan_excerpt" in record
    assert "body line" in record["plan_excerpt"]
    # Sanity: new fields coexist with deep-mode excerpt.
    assert record["size"] == "S"


# ---------------------------------------------------------------------------
# CLI integration: end-to-end JSON wire round-trip
# ---------------------------------------------------------------------------


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    """Empty graph routed to tmp_path via monkeypatch (matches sibling tests)."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


def _seed_entries(graph_path: Path, entries: list[dict]) -> None:
    """Write a hand-crafted graph.json so we can exercise legacy/edge fields."""
    graph_path.write_text(json.dumps({"entries": entries}))


def _invoke(*args):
    return runner.invoke(app, list(args), catch_exceptions=False)


def test_cli_context_candidate_round_trips_enriched_fields(tmp_graph, tmp_path):
    """`fno backlog triage context` JSON must carry every enriched field
    so a future refactor of cmd_context can't silently drop them.
    """
    plan = tmp_path / "p.md"
    plan.write_text("---\ntitle: P\n---\n# P\n")
    _seed_entries(
        tmp_graph,
        [
            {
                "id": "ab-CLI",
                "title": "CLI test",
                "priority": "p1",
                "plan_path": str(plan),
                "size": "M",
                "domain": "code",
                "details": "user-supplied implementation guidance",
                "cost_sessions": [{"cost_usd": 1.5}, {"cost_usd": 2.0}],
                "claimed_at": "2026-04-27T10:00:00Z",
                "pr_number": 42,
                "merge_status": "open",
                "status": "ready",
            }
        ],
    )

    r = _invoke("backlog", "triage", "context", "--all")
    assert r.exit_code == 0, r.output
    ctx = json.loads(r.stdout)
    assert ctx["candidates"], "expected one candidate from seeded graph"
    c = ctx["candidates"][0]

    assert c["id"] == "ab-CLI"
    assert c["size"] == "M"
    assert c["domain"] == "code"
    assert c["details"] == "user-supplied implementation guidance"
    assert c["claim_history"]["session_count"] == 2
    assert c["claim_history"]["total_cost_usd"] == 3.5
    assert c["claim_history"]["last_claimed_at"] == "2026-04-27T10:00:00Z"
    assert c["ship_state"]["pr_number"] == 42
    assert c["ship_state"]["merge_status"] == "open"


def test_cli_context_idea_branch_also_enriched(tmp_graph):
    """`_collect_ideas` must apply the same enrichment as `_collect_pending`.

    The unit tests only exercise `_candidate_record` once; this proves the
    ideas branch (a separate caller path through `cmd_context`) propagates
    the new fields too.
    """
    _seed_entries(
        tmp_graph,
        [
            {
                "id": "ab-IDEA",
                "title": "An idea",
                "priority": "p2",
                "plan_path": None,
                "size": "S",
                "details": "thought captured at intake time",
                "status": "idea",
            }
        ],
    )

    r = _invoke("backlog", "triage", "context", "--all")
    assert r.exit_code == 0, r.output
    ctx = json.loads(r.stdout)
    assert ctx["ideas"], "expected one idea from seeded graph"
    i = ctx["ideas"][0]

    assert i["id"] == "ab-IDEA"
    assert i["size"] == "S"
    assert i["details"] == "thought captured at intake time"
    assert "claim_history" in i
    assert "ship_state" in i


def test_cli_context_tolerates_legacy_dict_cost_sessions(tmp_graph):
    """Real graphs in the wild may carry a dict-shaped cost_sessions on
    older nodes; the CLI must not crash, and must report zero cost.
    """
    _seed_entries(
        tmp_graph,
        [
            {
                "id": "ab-LEGACY",
                "title": "Legacy node",
                "priority": "p2",
                "plan_path": "fake.md",
                "cost_sessions": {"sess-1": {"cost_usd": 5.0}},  # legacy dict
                "status": "ready",
            }
        ],
    )

    r = _invoke("backlog", "triage", "context", "--all")
    assert r.exit_code == 0, r.output
    ctx = json.loads(r.stdout)
    assert ctx["candidates"], "candidate should still surface despite legacy cost shape"
    c = ctx["candidates"][0]
    assert c["claim_history"]["session_count"] == 0
    assert c["claim_history"]["total_cost_usd"] == 0
