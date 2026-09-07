"""x-1379: the retirement verdict joins a worker row to its node's doneness.

A king reads ``fno agents top``, sees a provider lane at its cap, and cannot
tell that a holder's node already merged. These tests pin the one rule that
decides "has this worker's node shipped": done AND merged AND no additional
PR, resolved fail-closed from the graph.
"""

from __future__ import annotations

import pytest

from fno.agents.retirement import resolve_node, verdicts

IDS = {"x-7fbb", "x-ba96", "x-d15a", "x-dcf0", "x-feed", "x-abc1", "y-abc1"}


def _node(id, status="done", merge="merged", pr=None, extra=None):
    return {
        "id": id,
        "status": status,
        "merge_status": merge,
        "pr_number": pr,
        "additional_prs": extra or [],
    }


def test_done_merged_no_extra_pr_retires():
    """AC1-HP: the x-7fbb shape - the reason names the merged PR."""
    entries = [_node("x-7fbb", pr=1553)]
    out = verdicts([("t-7fbb-toby", None)], entries=entries)
    v = out["t-7fbb-toby"]
    assert v.retire is True
    assert v.node == "x-7fbb"
    assert v.node_basis == "name"
    assert v.reason == "done+merged PR 1553"


def test_done_merged_with_additional_pr_holds():
    """AC1-EDGE: the x-ba96 shape - the one test that stops this feature
    from telling a king to kill live work."""
    entries = [
        _node(
            "x-ba96",
            pr=1507,
            extra=[
                {"number": 1522, "url": "http://x/1522"},
                {"number": 1600, "url": "http://x/1600"},
            ],
        )
    ]
    out = verdicts([("t-ba96-w", None)], entries=entries)
    v = out["t-ba96-w"]
    assert v.retire is False
    assert v.reason == "extra-pr:1522,1600"


def test_done_without_merge_status_holds():
    entries = [_node("x-7fbb", merge=None, pr=1553)]
    (v,) = verdicts([("t-7fbb-toby", None)], entries=entries).values()
    assert v.retire is False
    assert v.reason == "merge=None"


def test_in_review_holds():
    entries = [_node("x-7fbb", status="in_review", merge=None)]
    (v,) = verdicts([("t-7fbb-toby", None)], entries=entries).values()
    assert v.retire is False
    assert v.reason == "status=in_review"


def test_unresolved_name_reads_no_node():
    """AC2-EDGE shape: an unresolvable row is a real answer, never a crash."""
    (v,) = verdicts([("just-a-king", None)], entries=[]).values()
    assert v.node is None
    assert v.node_basis is None
    assert v.retire is False
    assert v.reason == "no-node"


def test_name_resolution_token_forms():
    """The joined token-1+2 form and the bare-hex form both resolve; a hex
    word in a later token is never minted into an id."""
    assert resolve_node("target-x-dcf0-slug", None, IDS) == ("x-dcf0", "name")
    assert resolve_node("t-7fbb-toby", None, IDS) == ("x-7fbb", "name")
    # x-feed exists in the graph; `feed` is token 2 and is never consulted.
    assert resolve_node("t-d15a-feed-timeout", None, IDS) == ("x-d15a", "name")


def test_ambiguous_bare_hex_resolves_to_none():
    assert resolve_node("t-abc1-worker", None, IDS) == (None, None)


def test_registry_node_wins_and_stamps_basis():
    assert resolve_node("t-7fbb-toby", "x-ba96", IDS) == ("x-ba96", "registry")


def test_unreadable_graph_fails_closed_for_every_row(monkeypatch):
    """AC1-ERR: an unreadable graph is a hold for the whole roster, and the
    roster is never an empty dict (which would read as no rows)."""
    import fno.graph.load as graph_load

    def boom():
        raise RuntimeError("locked")

    monkeypatch.setattr(graph_load, "load_graph", boom)
    out = verdicts([("t-7fbb-toby", None), ("t-ba96-w", None)])
    assert sorted(out) == ["t-7fbb-toby", "t-ba96-w"]
    assert all(v.retire is False for v in out.values())
    assert all(v.reason.startswith("graph-unreadable") for v in out.values())


def test_missing_graph_file_fails_closed(monkeypatch):
    import fno.graph.load as graph_load

    monkeypatch.setattr(
        graph_load, "GRAPH_JSON", graph_load.Path("/nonexistent/graph.json")
    )
    out = verdicts([("t-7fbb-toby", None)])
    (v,) = out.values()
    assert v.retire is False
    assert v.reason.startswith("graph-unreadable")
