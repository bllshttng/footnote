"""Selection-time exclusion of nodes that already carry an open PR (ab-372130f6).

The bug: `fno backlog next` / `ready` (and therefore `advance` auto-continue and
megawalk, which both shell `fno backlog next`) re-select a node that already has
an open, unmerged PR. A `no-merge` node keeps `_status: ready` for the entire
review window (it only becomes `done` at merge), and its PID-based `node:<id>`
claim dies when the builder session exits - so the node looks like fresh ready
work and a redundant worker is dispatched.

The fix mirrors the existing `_live_claimed_node_ids()` exclusion with a new
`_has_unmerged_open_pr()` predicate applied at both selection sites.

Filter: `uv run pytest cli/tests/graph/test_open_pr_selection.py -q`
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.graph.cli import cli, _has_unmerged_open_pr


runner = CliRunner()


def _node(node_id: str, **overrides) -> dict:
    """A minimally-complete graph entry for the in-memory selectors.

    Defaults model a normal ready node (plan_path set, not claimed, not done,
    no PR). Override pr_number / merge_status / completed_at to model the
    in-review states.
    """
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
        "_status": "ready",
        "created_at": "2026-06-13T00:00:00+00:00",
    }
    base.update(overrides)
    return base


@pytest.fixture
def graph_file(tmp_path, monkeypatch):
    """Point the graph selectors at a temp graph.json and silence live claims.

    Returns a writer: call it with a list of entries to (re)write the graph.
    """
    path = tmp_path / "graph.json"

    def write(entries):
        # graph.json on disk is a JSON object keyed by "entries" (a bare list
        # is treated as corrupt by _read_json -> []), so wrap accordingly.
        path.write_text(json.dumps({"entries": entries}), encoding="utf-8")
        return path

    monkeypatch.setattr("fno.graph.cli._graph_path", lambda: path)
    # Never consult the real global claims root from a unit test.
    monkeypatch.setattr("fno.graph.cli._live_claimed_node_ids", lambda: set())
    return write


# ---------------------------------------------------------------------------
# Predicate unit coverage (Verification #1)
# ---------------------------------------------------------------------------


def test_predicate_open_unmerged_pr_excluded():
    """AC1-HP: a ready node with an open unmerged PR is in review, not ready."""
    assert _has_unmerged_open_pr(
        {"pr_number": 515, "merge_status": None, "completed_at": None}
    ) is True


def test_predicate_merged_but_not_closed_excluded():
    """Verification #1: merged-but-pre-close stays excluded (reconcile pending).

    A PR that merged but whose node has not been closed yet (completed_at
    still None) must NOT be re-dispatched - the work is already done, the
    close just has not landed. (This is where the plan's illustrative snippet
    `merge_status != "merged"` was wrong; the criteria require True here.)
    """
    assert _has_unmerged_open_pr(
        {"pr_number": 515, "merge_status": "merged", "completed_at": None}
    ) is True


def test_predicate_no_pr_is_genuinely_ready():
    """AC1-ERR: the normal case - no PR - is never excluded."""
    assert _has_unmerged_open_pr(
        {"pr_number": None, "merge_status": None, "completed_at": None}
    ) is False


def test_predicate_done_node_not_excluded():
    """AC2-HP: a closed node is already out of the ready pool; do not interfere."""
    assert _has_unmerged_open_pr(
        {"pr_number": 515, "merge_status": None, "completed_at": "2026-06-13T01:00:00+00:00"}
    ) is False


def test_predicate_missing_keys_default_ready():
    """Robustness: an entry missing pr_number/completed_at entirely is ready."""
    assert _has_unmerged_open_pr({}) is False


# ---------------------------------------------------------------------------
# `fno backlog next` selector (Verification #2, AC1-EDGE)
# ---------------------------------------------------------------------------


def test_next_skips_node_with_open_pr(graph_file):
    """AC1-EDGE: when the only ready node has an open PR, `next` returns null."""
    graph_file([_node("ab-aaaa1111", pr_number=515)])
    result = runner.invoke(cli, ["next", "--all"])
    assert result.exit_code == 0
    assert result.stdout.strip() == "null"


def test_next_returns_no_pr_sibling(graph_file):
    """`next` still selects a sibling node that has no PR."""
    graph_file([
        _node("ab-aaaa1111", pr_number=515),
        _node("ab-bbbb2222"),
    ])
    result = runner.invoke(cli, ["next", "--all"])
    assert result.exit_code == 0
    picked = json.loads(result.stdout)
    assert picked is not None
    assert picked["id"] == "ab-bbbb2222"


def test_next_unaffected_when_no_open_prs(graph_file):
    """A normal graph with no PRs is unchanged: `next` returns the ready node."""
    graph_file([_node("ab-cccc3333")])
    result = runner.invoke(cli, ["next", "--all"])
    assert result.exit_code == 0
    picked = json.loads(result.stdout)
    assert picked["id"] == "ab-cccc3333"


# ---------------------------------------------------------------------------
# `fno backlog ready` selector (Verification #3)
# ---------------------------------------------------------------------------


def test_ready_omits_open_pr_node_lists_sibling(graph_file):
    """Verification #3: `ready` hides the open-PR'd node, lists the no-PR one."""
    graph_file([
        _node("ab-aaaa1111", pr_number=515),
        _node("ab-bbbb2222"),
    ])
    result = runner.invoke(cli, ["ready", "--all"])
    assert result.exit_code == 0
    listed_ids = {e["id"] for e in json.loads(result.stdout)}
    assert "ab-aaaa1111" not in listed_ids
    assert "ab-bbbb2222" in listed_ids


def test_ready_merged_pre_close_node_omitted(graph_file):
    """A merged-but-not-yet-closed node is also omitted from `ready`."""
    graph_file([_node("ab-dddd4444", pr_number=515, merge_status="merged")])
    result = runner.invoke(cli, ["ready", "--all"])
    assert result.exit_code == 0
    assert json.loads(result.stdout) == []


# ---------------------------------------------------------------------------
# Defer-contract parity (codex PR #516 P2): the open-PR guard is scoped to
# normal ready selection. An operator who EXPLICITLY surfaces paused nodes via
# --include-deferred must still see a deferred node that happens to carry a PR;
# the guard only suppresses AUTO re-selection of fresh ready work.
# ---------------------------------------------------------------------------


def test_ready_include_deferred_keeps_pr_bearing_deferred_node(graph_file):
    """`ready --include-deferred` still lists a deferred node that has a PR."""
    graph_file([
        _node(
            "ab-eeee5555",
            pr_number=515,
            _status="deferred",
            deferred_at="2026-06-13T02:00:00+00:00",
            deferred_reason="paused by operator",
        ),
    ])
    result = runner.invoke(cli, ["ready", "--all", "--include-deferred"])
    assert result.exit_code == 0
    listed_ids = {e["id"] for e in json.loads(result.stdout)}
    assert "ab-eeee5555" in listed_ids


def test_next_include_deferred_keeps_pr_bearing_deferred_node(graph_file):
    """`next --include-deferred` can re-engage a deferred node that has a PR."""
    graph_file([
        _node(
            "ab-eeee5555",
            pr_number=515,
            _status="deferred",
            deferred_at="2026-06-13T02:00:00+00:00",
            deferred_reason="paused by operator",
        ),
    ])
    result = runner.invoke(cli, ["next", "--all", "--include-deferred"])
    assert result.exit_code == 0
    picked = json.loads(result.stdout)
    assert picked is not None
    assert picked["id"] == "ab-eeee5555"


def test_ready_default_still_omits_deferred_pr_node(graph_file):
    """Without --include-deferred, the deferred node is absent anyway (status
    filter), so the scoping change does not leak it into the normal listing."""
    graph_file([
        _node("ab-eeee5555", pr_number=515, _status="deferred"),
        _node("ab-ffff6666", pr_number=515),  # ready + open PR -> excluded
        _node("ab-aaaa7777"),                  # ready, no PR -> listed
    ])
    result = runner.invoke(cli, ["ready", "--all"])
    assert result.exit_code == 0
    listed_ids = {e["id"] for e in json.loads(result.stdout)}
    assert listed_ids == {"ab-aaaa7777"}
