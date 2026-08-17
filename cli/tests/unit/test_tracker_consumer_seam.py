"""Per-reader-class sentinel tests for consumers routed through the seam.

The migration standard (plan task 2.1): a consumer that reads ONLY
seam-owned fields stops touching the graph file. Each test here gives the
tracker side and the sidecar side DIFFERENT sentinel values than the graph
file carries, so a pass is positive evidence the reader went through the
seam - an assertion that ``read_graph`` text disappeared is not evidence
(plan Risk 3 / the king's census requirement).
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest


@pytest.fixture
def external_store(tmp_path, monkeypatch):
    """External mode with per-id sidecar files; returns the sidecar dir."""
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    sidecars = tmp_path / "sidecars"
    sidecars.mkdir()
    import fno.tracker.sidecar as sidecar_store

    monkeypatch.setattr(
        sidecar_store, "sidecar_path", lambda i: sidecars / f"{i}.json"
    )
    return sidecars


@pytest.fixture
def contradictory_graph(tmp_path, monkeypatch):
    """A graph file whose every value CONTRADICTS the sidecar sentinels."""
    g = tmp_path / "graph.json"
    g.write_text(
        json.dumps({"entries": [
            {"id": "N-1", "pr_number": 999, "pr_url": "https://graph/999",
             "cwd": "/graph-cwd", "size": "GRAPH-SIZE", "source_cwd": "/graph-src"},
            {"id": "N-2", "pr_number": 999, "pr_url": "https://graph/999b",
             "cwd": "/graph-cwd", "size": None},
        ]}),
        encoding="utf-8",
    )
    # Patch the resolver, not _constants.GRAPH_JSON: monkeypatch teardown
    # concretizes that lazy attr, and a frozen value would poison every later
    # paths.graph_json redirect in the same process (sidecar._graph_store_path
    # reads the resolver directly for exactly this reason).
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


def _write_sidecar(sidecars: Path, id: str, **fields) -> None:
    payload = {"id": id}
    payload.update(fields)
    (sidecars / f"{id}.json").write_text(json.dumps(payload), encoding="utf-8")


def test_pr_scan_class_reads_sidecar_not_graph(external_store, contradictory_graph):
    """Reader class: scan-by-PR (mail job addressing, pr merge node resolve)."""
    _write_sidecar(external_store, "N-1", pr_number=7,
                   pr_url="https://ext/7", cwd="/ext-cwd")
    _write_sidecar(external_store, "N-2",
                   additional_prs=[{"number": 7, "url": "https://ext/7b"}])
    _write_sidecar(external_store, "N-3", pr_number=None)

    from fno.mail.job_address import _node_ids_for_pr

    ids = _node_ids_for_pr(7)
    # Both PR-bearing sidecars found (primary + additional); the graph's 999s
    # are invisible, and a PR the store does not carry resolves to nothing.
    assert set(ids) == {"N-1", "N-2"}
    assert _node_ids_for_pr(999) == []


def test_pr_node_resolver_runs_over_sidecar_rows(external_store, contradictory_graph):
    """Reader class: repo-scoped PR->node resolution (pr merge close path)."""
    from fno.pr._merge import _find_pr_node_id
    from fno.tracker import sidecar as sidecar_store

    _write_sidecar(external_store, "N-1", pr_number=7, pr_url="https://ext/7")
    rows = [
        {"id": nid, "pr_number": sc.pr_number, "pr_url": sc.pr_url,
         "additional_prs": sc.additional_prs}
        for nid, sc in sidecar_store.load_all().items()
    ]
    # Url match resolves through the sidecar sentinel, not the graph's 999.
    assert _find_pr_node_id(rows, 7, "https://ext/7") == "N-1"
    # The graph-only number never resolves.
    assert _find_pr_node_id(rows, 999, "https://graph/999") is None


def test_cwd_roots_scan_reads_sidecar(external_store, contradictory_graph, tmp_path):
    """Reader class: cwd/source_cwd root scan (outstanding capture collection)."""
    live = tmp_path / "live-root"
    live.mkdir()
    _write_sidecar(external_store, "N-1", cwd=str(live), source_cwd="/absent")
    from fno.outstanding.core import _capture_project_roots

    roots = _capture_project_roots(tmp_path)
    assert live.resolve() in [Path(r).resolve() for r in roots]
    # The graph's /graph-cwd is not a directory and never appears; a real
    # caller root always rides along.
    assert str(tmp_path) in roots or Path(tmp_path).resolve() in [Path(r).resolve() for r in roots]


def test_node_size_is_guarded_metadata(external_store, contradictory_graph, monkeypatch, tmp_path):
    """Reader class: footnote-minted metadata (size pin) reads the default
    store through the guarded reader and never leaks under an external
    backend - the graph carries GRAPH-SIZE, external must yield None."""
    import fno.worker.review as review_mod

    monkeypatch.setattr(
        "fno.worker.ship._read_graph_node_id", lambda _sp: "N-1"
    )
    assert review_mod._resolve_node_size(tmp_path / "state.md") is None
    # Graph mode reads the same pin it always did (byte-compat, AC1).
    monkeypatch.delenv("FNO_TRACKER_BACKEND", raising=False)
    assert review_mod._resolve_node_size(tmp_path / "state.md") == "GRAPH-SIZE"


def test_graph_mode_scans_project_from_the_store(tmp_path, monkeypatch):
    """Graph-mode parity for the scan class: the sidecar projection reads the
    footnote-owned fields out of the graph entry, so the same callers keep
    finding what they found before the migration."""
    monkeypatch.delenv("FNO_TRACKER_BACKEND", raising=False)
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [
        {"id": "ab-1", "pr_number": 42, "cwd": "/repo", "size": "L"},
        {"id": "ab-2"},
    ]}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)

    from fno.mail.job_address import _node_ids_for_pr
    from fno.tracker import sidecar as sidecar_store

    assert _node_ids_for_pr(42) == ["ab-1"]
    all_loaded = sidecar_store.load_all()
    assert all_loaded["ab-1"].cwd == "/repo"
    # The size pin is footnote-minted metadata, NOT a sidecar field: the
    # projection must not carry it even though the entry does.
    assert "size" not in type(all_loaded["ab-1"]).model_fields
    assert all_loaded["ab-2"].pr_number is None
