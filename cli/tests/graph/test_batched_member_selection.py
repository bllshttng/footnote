"""Selection-time exclusion of open-batch members (batch-lane Wave 2, x-6cdf).

A node committed to an open batch has its atomic commits on a shared batch
branch and ships as part of the batch PR, not its own. `fno backlog next` /
`ready` (and therefore the auto-continue daemon + advance, which shell them)
must NOT re-select it - else a second worker rebuilds work already on the
branch. The mark is the graph `batch` field, set by `/target batched` via
`fno backlog update --batch <id>` and cleared (`--batch null`) on abandon so a
requeued member resurfaces.

Mirrors test_open_pr_selection.py: a new `_is_batched_member()` predicate applied
at both selection sites, plus the `fno backlog update --batch` marker.

Filter: `uv run pytest cli/tests/graph/test_batched_member_selection.py -q`
"""
from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone

import pytest
from typer.testing import CliRunner

from fno.graph.cli import cli, _is_batched_member


runner = CliRunner()

# Recent so the G1 stale-ready guard never quarantines these selection
# fixtures (a hardcoded past date rots past the threshold as wall-clock moves).
_RECENT_CREATED = (datetime.now(timezone.utc) - timedelta(days=1)).isoformat()


def _node(node_id: str, **overrides) -> dict:
    base = {
        "id": node_id,
        "title": f"node {node_id}",
        "plan_path": f"plans/{node_id}.md",
        "priority": "p2",
        "rank": None,
        "type": "feature",
        "parent": None,
        "project": "fno",
        "domain": "code",
        "blocked_by": [],
        "session_id": None,
        "claimed_at": None,
        "completed_at": None,
        "pr_number": None,
        "merge_status": None,
        "batch": None,
        "_status": "ready",
        "created_at": _RECENT_CREATED,
    }
    base.update(overrides)
    return base


@pytest.fixture
def graph_file(tmp_path, monkeypatch):
    path = tmp_path / "graph.json"

    def write(entries):
        path.write_text(json.dumps({"entries": entries}), encoding="utf-8")
        return path

    monkeypatch.setattr("fno.graph.cli._graph_path", lambda: path)
    monkeypatch.setattr("fno.graph.cli._live_claimed_node_ids", lambda: set())
    return write


# ---------------------------------------------------------------------------
# Predicate unit coverage
# ---------------------------------------------------------------------------


def test_predicate_batch_set_excluded():
    """AC1-HP: a node with a batch id set is an open-batch member, not ready."""
    assert _is_batched_member({"batch": "batch-ab12cd34"}) is True


def test_predicate_no_batch_is_ready():
    """AC1-ERR: the normal case - batch None - is never excluded."""
    assert _is_batched_member({"batch": None}) is False


def test_predicate_missing_key_is_ready():
    """Robustness: an entry missing `batch` entirely is ready."""
    assert _is_batched_member({}) is False


def test_predicate_empty_string_is_ready():
    """An empty-string batch id is a cleared mark (never a real batch id)."""
    assert _is_batched_member({"batch": ""}) is False


# ---------------------------------------------------------------------------
# `fno backlog next` selector
# ---------------------------------------------------------------------------


def test_next_skips_batched_member(graph_file):
    """AC1-EDGE: when the only ready node is a batch member, `next` returns null."""
    graph_file([_node("ab-aaaa1111", batch="batch-ab12cd34")])
    result = runner.invoke(cli, ["next", "--all"])
    assert result.exit_code == 0
    assert result.stdout.strip() == "null"


def test_next_returns_unbatched_sibling(graph_file):
    """`next` still selects a sibling node that is not batched."""
    graph_file([
        _node("ab-aaaa1111", batch="batch-ab12cd34"),
        _node("ab-bbbb2222"),
    ])
    result = runner.invoke(cli, ["next", "--all"])
    assert result.exit_code == 0
    picked = json.loads(result.stdout)
    assert picked is not None
    assert picked["id"] == "ab-bbbb2222"


# ---------------------------------------------------------------------------
# `fno backlog ready` selector
# ---------------------------------------------------------------------------


def test_ready_omits_batched_member_lists_sibling(graph_file):
    """`ready` hides the batched node, lists the unbatched one."""
    graph_file([
        _node("ab-aaaa1111", batch="batch-ab12cd34"),
        _node("ab-bbbb2222"),
    ])
    result = runner.invoke(cli, ["ready", "--all"])
    assert result.exit_code == 0
    listed_ids = {e["id"] for e in json.loads(result.stdout)}
    assert "ab-aaaa1111" not in listed_ids
    assert "ab-bbbb2222" in listed_ids


# ---------------------------------------------------------------------------
# `fno backlog update --batch` marker (mark + clear-on-abandon)
# ---------------------------------------------------------------------------


def test_update_batch_marks_node(graph_file):
    """`update --batch <id>` sets node.batch so selection then excludes it."""
    graph_file([_node("ab-cccc3333")])
    r = runner.invoke(cli, ["update", "ab-cccc3333", "--batch", "batch-ffff9999"])
    assert r.exit_code == 0, r.stdout
    # The mark now hides it from `next`.
    picked = runner.invoke(cli, ["next", "--all"]).stdout.strip()
    assert picked == "null"


def test_update_batch_null_clears_mark(graph_file):
    """`update --batch null` requeues a member (resurfaces in selection)."""
    graph_file([_node("ab-cccc3333", batch="batch-ffff9999")])
    r = runner.invoke(cli, ["update", "ab-cccc3333", "--batch", "null"])
    assert r.exit_code == 0, r.stdout
    picked = json.loads(runner.invoke(cli, ["next", "--all"]).stdout)
    assert picked is not None
    assert picked["id"] == "ab-cccc3333"
