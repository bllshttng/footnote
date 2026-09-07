"""Read-only discovery pass tests.

The pass is deliberately tested through both its small Python seam and the
backlog CLI.  The graph bytes are the mutation oracle: a discovery run may
build a disposable FTS cache, but it must never rewrite graph state.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.graph import discovery, fts
from fno.graph.cli import cli

runner = CliRunner()


def _node(node_id: str, **overrides: object) -> dict:
    base = {
        "id": node_id,
        "title": f"node {node_id}",
        "slug": f"node-{node_id}",
        "details": "",
        "project": "fno",
        "type": "feature",
        "parent": None,
        "priority": "p2",
        "status": "ready",
        "blocked_by": [],
        "completed_at": None,
        "pr_number": None,
        "pr_url": None,
        "children": [],
    }
    base.update(overrides)
    return base


@pytest.fixture()
def tmp_graph(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    from fno.graph import _constants as constants
    from fno.graph import store

    graph = tmp_path / "graph.json"
    graph.write_text(json.dumps({"entries": []}) + "\n", encoding="utf-8")
    monkeypatch.setattr(constants, "GRAPH_JSON", graph)
    monkeypatch.setattr(constants, "GRAPH_ARCHIVE_JSON", tmp_path / "archive.json")
    monkeypatch.setattr(store, "GRAPH_JSON", graph)
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr("fno.paths.graph_archive_json", lambda: tmp_path / "archive.json")
    return graph


def _seed(graph: Path, entries: list[dict]) -> None:
    graph.write_text(json.dumps({"entries": entries}) + "\n", encoding="utf-8")


def test_candidates_union_recall_lanes(tmp_graph: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _seed(tmp_graph, [_node("x-fts"), _node("x-related"), _node("x-both")])
    monkeypatch.setattr(fts, "search", lambda *args, **kwargs: ["x-fts", "x-both"])
    monkeypatch.setattr(
        "fno.graph.relatedness.similar_nodes",
        lambda *args, **kwargs: [
            ("x-related", 0.7, "related terms"),
            ("x-both", 0.4, "shared terms"),
        ],
    )

    result = discovery.candidates("candidate", "details", graph_path=tmp_graph)

    assert [candidate.node_id for candidate in result] == ["x-related", "x-both", "x-fts"]
    assert result[0].lanes == frozenset({"relatedness"})
    assert result[1].lanes == frozenset({"fts", "relatedness"})
    assert result[2].lanes == frozenset({"fts"})
    assert result[2].score == 0.1


def test_candidates_degrade_when_fts_is_unavailable(
    tmp_graph: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _seed(tmp_graph, [_node("x-related")])

    def unavailable(*args, **kwargs):
        raise fts.SearchUnavailableError("no fts5")

    monkeypatch.setattr(fts, "search", unavailable)
    monkeypatch.setattr(
        "fno.graph.relatedness.similar_nodes",
        lambda *args, **kwargs: [("x-related", 0.4, "shared terms")],
    )

    result = discovery.candidates("candidate", "details", graph_path=tmp_graph)

    assert result.degraded is True
    assert result.warning == "no fts5"
    assert result[0].lanes == frozenset({"relatedness"})


def test_assess_cites_duplicate_and_satisfied_evidence() -> None:
    duplicate = discovery.assess(
        _node("x-expired", status="deferred"),
        [discovery.Candidate("x-done", 0.6, frozenset({"fts"}))],
    )
    satisfied = discovery.assess(
        _node("x-expired", status="deferred", pr_number=1345, completed_at="2026-09-01T00:00:00Z"),
        [],
    )

    assert duplicate.verdict == "duplicate"
    assert "x-done" in duplicate.evidence
    assert satisfied.verdict == "satisfied"
    assert "PR#1345" in satisfied.evidence


def test_assess_unknown_explains_why() -> None:
    result = discovery.assess(
        _node("x-expired", status="deferred", deferred_reason="operator decision"),
        [],
    )

    assert result.verdict == "undecided"
    assert result.reason
    assert result.evidence == []


def test_discover_expired_population_is_read_only(tmp_graph: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _seed(
        tmp_graph,
        [
            _node("x-expired", status="deferred", deferred_kind="expired", deferred_reason="stale >30d, drained by maintain"),
            _node("x-decided", status="deferred", deferred_kind="wont_do", deferred_reason="operator decision"),
            _node("x-ready", status="ready"),
        ],
    )
    monkeypatch.setattr(discovery, "candidates", lambda *args, **kwargs: discovery.CandidateResults())
    before = tmp_graph.read_bytes()

    result = runner.invoke(cli, ["discover", "--json"])

    assert result.exit_code == 0, result.output
    report = json.loads(result.stdout)
    assert report["assessed"] == 1
    assert report["excluded_by_kind"] == 1
    assert report["worklist"][0]["verdict"] == "undecided"
    assert tmp_graph.read_bytes() == before


def test_discover_none_match_reports_positive_control(tmp_graph: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _seed(tmp_graph, [_node("x-expired", status="deferred", deferred_kind="expired")])
    monkeypatch.setattr(discovery, "candidates", lambda *args, **kwargs: discovery.CandidateResults())

    result = runner.invoke(cli, ["discover", "--json"])

    assert result.exit_code == 0, result.output
    report = json.loads(result.stdout)
    assert len(report["worklist"]) == 1
    assert report["worklist"][0]["candidates"] == []
    assert report["positive_control"]["matches"] == ["x-expired"]


def test_discover_refuses_all_match_fixture(tmp_graph: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _seed(
        tmp_graph,
        [
            _node("x-one", status="deferred", deferred_kind="expired"),
            _node("x-two", status="deferred", deferred_kind="expired"),
        ],
    )
    monkeypatch.setattr(
        discovery,
        "candidates",
        lambda *args, **kwargs: discovery.CandidateResults(
            [discovery.Candidate("x-other", 0.5, frozenset({"fts"}))]
        ),
    )

    result = runner.invoke(cli, ["discover"])

    assert result.exit_code != 0
    assert "failed instrument" in result.output.lower()


def test_idea_fold_includes_fts_only_lane(tmp_graph: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _seed(tmp_graph, [_node("x-fts", title="Vocabulary match", status="ready")])
    monkeypatch.setattr(
        discovery,
        "candidates",
        lambda *args, **kwargs: discovery.CandidateResults(
            [discovery.Candidate("x-fts", 0.12, frozenset({"fts"}), "vocabulary match")]
        ),
    )
    monkeypatch.setattr(
        "fno.graph.relatedness.filing_candidates",
        lambda entries, sidecar: (entries, "fixture"),
    )

    result = runner.invoke(cli, ["idea", "Vocabulary filing", "--difficulty", "low", "--json"])

    assert result.exit_code == 0, result.output
    receipt = json.loads(result.stdout)
    assert receipt["candidates"][0]["lanes"] == ["fts"]
    assert "fts-only" in receipt["candidates"][0]["evidence"]
