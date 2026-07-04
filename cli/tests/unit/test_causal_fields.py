"""W4 causal links on graph nodes (x-aff6, task 4.2).

Covers the three fields (caused_by / fixes_pr / reverted), their
`backlog update` flags, the retro-land auto-caused_by threading, and
reconcile's pure revert detection.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.graph._reconcile import detect_reverted_nodes
from fno.graph.types import Entry
from fno.retro.land import land_candidates
from fno.retro.types import TIER_NODE, Candidate

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _read(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


def _node(nid: str, **extra) -> dict:
    return {"id": nid, "title": nid, "domain": "code", "project": "p", **extra}


# -- Entry model --------------------------------------------------------------


def test_entry_carries_causal_fields():
    e = Entry(id="x-0001", caused_by="x-0002", fixes_pr=42, reverted=True)
    assert (e.caused_by, e.fixes_pr, e.reverted) == ("x-0002", 42, True)


def test_entry_causal_defaults_parse_old_graphs():
    e = Entry(id="x-0001")
    assert (e.caused_by, e.fixes_pr, e.reverted) == (None, None, False)


# -- backlog update flags ------------------------------------------------------


def test_update_sets_causal_fields(tmp_graph):
    _seed(tmp_graph, [_node("ab-00000001"), _node("ab-00000002")])
    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001",
        "--caused-by", "ab-00000002", "--fixes-pr", "42", "--reverted",
    ])
    assert result.exit_code == 0, result.output
    n = _read(tmp_graph)[0]
    assert n["caused_by"] == "ab-00000002"
    assert n["fixes_pr"] == 42
    assert n["reverted"] is True


def test_update_caused_by_self_reference_fails(tmp_graph):
    _seed(tmp_graph, [_node("ab-00000001")])
    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--caused-by", "ab-00000001",
    ])
    assert result.exit_code == 1
    assert _read(tmp_graph)[0].get("caused_by") is None


def test_update_caused_by_unknown_node_fails(tmp_graph):
    _seed(tmp_graph, [_node("ab-00000001")])
    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001", "--caused-by", "ab-deadbeef",
    ])
    assert result.exit_code == 1


def test_update_clears_causal_fields(tmp_graph):
    _seed(tmp_graph, [
        _node("ab-00000001", caused_by="ab-00000002", fixes_pr=42, reverted=True),
        _node("ab-00000002"),
    ])
    result = runner.invoke(app, [
        "backlog", "update", "ab-00000001",
        "--caused-by", "null", "--fixes-pr", "0", "--no-reverted",
    ])
    assert result.exit_code == 0, result.output
    n = _read(tmp_graph)[0]
    assert n["caused_by"] is None
    assert n["fixes_pr"] is None
    assert n["reverted"] is False


# -- retro land: auto caused_by ------------------------------------------------


def _candidate() -> Candidate:
    return Candidate(
        title="follow-up",
        body="body",
        tier=TIER_NODE,
        priority="p2",
        source_pr=42,
        source_id="c1",
    )


def test_land_threads_caused_by_to_create(tmp_path):
    seen: list[dict] = []

    def create(**kw):
        seen.append(kw)
        return "ab-new1"

    land_candidates(
        [_candidate()], mode="autonomous", repo_root=tmp_path,
        create_fn=create, caused_by="x-orig",
    )
    assert seen[0]["caused_by"] == "x-orig"


def test_land_omits_caused_by_when_unknown(tmp_path):
    """A fixed-signature create_fn (no caused_by kwarg) stays call-compatible."""
    seen: list[str] = []

    def create(*, title, details, priority, project, cwd, domain="code", queued=False):
        seen.append(title)
        return "ab-new1"

    results = land_candidates(
        [_candidate()], mode="autonomous", repo_root=tmp_path, create_fn=create,
    )
    assert seen == ["follow-up"]
    assert results[0].outcome == "active"


def test_default_create_stamps_caused_by_in_graph(tmp_graph):
    """AC4-UI: the durable node in the graph carries the causal link."""
    from fno.retro.land import _default_create

    _seed(tmp_graph, [_node("ab-00000001")])
    nid = _default_create(
        title="follow-up", details="d", priority="p2",
        project=None, cwd=None, caused_by="ab-00000001",
    )
    created = next(n for n in _read(tmp_graph) if n["id"] == nid)
    assert created["caused_by"] == "ab-00000001"


def test_default_create_skips_stale_caused_by(tmp_graph):
    """A sentinel naming a node that no longer exists lands without the link."""
    from fno.retro.land import _default_create

    nid = _default_create(
        title="follow-up", details="d", priority="p2",
        project=None, cwd=None, caused_by="ab-deadbeef",
    )
    created = next(n for n in _read(tmp_graph) if n["id"] == nid)
    assert created.get("caused_by") is None


def test_triage_caused_by_falls_back_to_pr_number_match(tmp_path):
    """No sentinel node_id: the origin resolves from the repo-scoped pr match."""
    from fno.retro.routine import triage_pr

    seen: list[dict] = []

    def create(**kw):
        seen.append(kw)
        return "ab-new1"

    comments = [{"id": "c1", "body": "![high] real finding", "reviewer": "g[bot]"}]
    triage_pr(
        repo_root=tmp_path,
        pr_number=42,
        mode="autonomous",
        comments=comments,
        existing_nodes=[
            {"id": "ab-other1", "pr_number": 42,
             "pr_url": "https://github.com/other/repo/pull/42"},
            {"id": "ab-origin1", "pr_number": 42,
             "pr_url": "https://github.com/o/r/pull/42"},
        ],
        repo="o/r",
        create_fn=create,
    )
    assert seen and seen[0]["caused_by"] == "ab-origin1"


# -- reconcile revert detection --------------------------------------------------


def _pr_url(n: int, slug: str = "o/r") -> str:
    return f"https://github.com/{slug}/pull/{n}"


def test_detect_reverted_nodes_matches_body_ref():
    entries = [
        _node("x-aaaa", pr_number=42, pr_url=_pr_url(42)),
        _node("x-bbbb", pr_number=7, pr_url=_pr_url(7)),
    ]
    merged = [{
        "number": 50,
        "title": 'Revert "feat: thing"',
        "body": "Reverts o/r#42",
        "url": _pr_url(50),
    }]
    assert detect_reverted_nodes(merged, entries) == [("x-aaaa", 50)]


def test_detect_reverted_nodes_ignores_non_revert_titles():
    entries = [_node("x-aaaa", pr_number=42, pr_url=_pr_url(42))]
    merged = [{"number": 50, "title": "feat: mentions #42", "body": "see #42",
               "url": _pr_url(50)}]
    assert detect_reverted_nodes(merged, entries) == []


def test_detect_reverted_nodes_skips_already_stamped():
    entries = [_node("x-aaaa", pr_number=42, pr_url=_pr_url(42), reverted=True)]
    merged = [{"number": 50, "title": "Revert x", "body": "reverts #42",
               "url": _pr_url(50)}]
    assert detect_reverted_nodes(merged, entries) == []


def test_detect_reverted_nodes_matches_additional_prs():
    entries = [_node("x-aaaa", additional_prs=[{"number": 43, "url": _pr_url(43)}])]
    merged = [{"number": 51, "title": 'Revert "fix"', "body": "This reverts #43.",
               "url": _pr_url(51)}]
    assert detect_reverted_nodes(merged, entries) == [("x-aaaa", 51)]


def test_detect_reverted_nodes_never_crosses_repos():
    """The graph is global: a same-numbered PR in another repo must not match."""
    entries = [_node("x-aaaa", pr_number=42, pr_url=_pr_url(42, "other/repo"))]
    merged = [{"number": 50, "title": 'Revert "x"', "body": "Reverts #42",
               "url": _pr_url(50)}]
    assert detect_reverted_nodes(merged, entries) == []


def test_detect_reverted_nodes_cross_repo_qualifier_skipped():
    """`Reverts other/repo#42` in the body names a foreign repo's PR."""
    entries = [_node("x-aaaa", pr_number=42, pr_url=_pr_url(42))]
    merged = [{"number": 50, "title": 'Revert "x"', "body": "Reverts other/repo#42",
               "url": _pr_url(50)}]
    assert detect_reverted_nodes(merged, entries) == []


def test_detect_reverted_nodes_ambiguous_match_stamps_nothing():
    entries = [
        _node("x-aaaa", pr_number=42, pr_url=_pr_url(42)),
        _node("x-bbbb", additional_prs=[{"number": 42, "url": _pr_url(42)}]),
    ]
    merged = [{"number": 50, "title": 'Revert "x"', "body": "Reverts #42",
               "url": _pr_url(50)}]
    assert detect_reverted_nodes(merged, entries) == []


def test_detect_reverted_nodes_skips_numberless_gh_row():
    entries = [_node("x-aaaa", pr_number=42, pr_url=_pr_url(42))]
    merged = [{"title": 'Revert "x"', "body": "Reverts #42", "url": _pr_url(50)}]
    assert detect_reverted_nodes(merged, entries) == []
