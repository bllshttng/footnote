"""The discover worklist's satisfied arm, domain plumbing, and control refusal.

Deferral clears every node-side completion field, so satisfaction on an
expired row must ride evidence that survives the defer: a gh-verified merged
PR or files recorded in the node's text that still exist.
"""
from __future__ import annotations

from fno.graph.discovery import (
    Assessment,
    Candidate,
    CandidateResults,
    assess,
    candidates,
)


def _node(**over):
    node = {
        "id": "x-1",
        "title": "expired thing",
        "status": "deferred",
        "deferred_kind": "expired",
    }
    node.update(over)
    return node


def test_verified_merged_pr_satisfies_deferred_row():
    node = _node(pr_number=7)
    assert assess(node, [], pr_state=lambda n: True).verdict == "satisfied"


def test_unverified_pr_number_does_not_satisfy_deferred_row():
    node = _node(pr_number=7)
    assert assess(node, [], pr_state=lambda n: False).verdict != "satisfied"
    assert assess(node, []).verdict != "satisfied"


def test_file_evidence_surviving_deferral_satisfies():
    node = _node(pr_number=7, details="the work lives in cli/src/fno/graph/discovery.py")
    verdict = assess(node, [], pr_state=lambda n: False)
    assert verdict.verdict == "satisfied"
    assert verdict.evidence


def test_candidates_passes_the_node_domain_to_relatedness(monkeypatch, tmp_path):
    seen = {}
    graph_file = tmp_path / "graph.json"
    graph_file.write_text("[]")

    def fake_similar(incoming, pool, **kwargs):
        seen["domain"] = incoming["domain"]
        return []

    monkeypatch.setattr("fno.graph.relatedness.similar_nodes", fake_similar)
    candidates(
        "t",
        "",
        entries=[{"id": "a", "title": "a"}],
        graph_path=graph_file,
        domain="docs",
    )
    assert seen["domain"] == "docs"


def test_candidates_default_domain_stays_code(monkeypatch, tmp_path):
    seen = {}
    graph_file = tmp_path / "graph.json"
    graph_file.write_text("[]")

    def fake_similar(incoming, pool, **kwargs):
        seen["domain"] = incoming["domain"]
        return []

    monkeypatch.setattr("fno.graph.relatedness.similar_nodes", fake_similar)
    candidates("t", "", entries=[{"id": "a", "title": "a"}], graph_path=graph_file)
    assert seen["domain"] == "code"


def _invoke_discover(monkeypatch, tmp_path, control_matches):
    import fno.graph._constants as constants
    from fno.graph import discovery
    from typer.testing import CliRunner
    from fno.graph.cli import cli

    graph_file = tmp_path / "graph.json"
    graph_file.write_text("[]")
    monkeypatch.setattr(constants, "GRAPH_JSON", graph_file)
    monkeypatch.setattr("fno.graph.store.read_graph", lambda path: [_node()])
    monkeypatch.setattr(discovery, "candidates", lambda *a, **k: CandidateResults())
    monkeypatch.setattr(
        discovery, "assess", lambda *a, **k: Assessment("undecided", [], "r")
    )
    monkeypatch.setattr(
        discovery,
        "positive_control",
        lambda *a, **k: {
            "query": "q",
            "matches": control_matches,
            "lane": "fts",
            "degraded": False,
        },
    )
    return CliRunner().invoke(cli, ["discover", "--json"])


def test_discover_refuses_when_the_positive_control_matches_nothing(
    monkeypatch, tmp_path
):
    result = _invoke_discover(monkeypatch, tmp_path, control_matches=[])
    assert result.exit_code == 2
    assert "positive control" in result.output


def test_discover_reports_when_the_positive_control_fires(monkeypatch, tmp_path):
    result = _invoke_discover(
        monkeypatch, tmp_path, control_matches=["x-0"]
    )
    assert result.exit_code == 0
    assert '"undecided"' in result.output


def test_candidate_shape_is_stable():
    c = Candidate("a", 0.5, frozenset({"fts"}))
    assert CandidateResults([c])[0].as_dict()["id"] == "a"
