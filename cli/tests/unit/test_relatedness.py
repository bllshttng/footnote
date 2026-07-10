"""Unit tests for the deterministic relatedness map (node x-c2e9)."""
from __future__ import annotations

import json
from datetime import datetime, timezone

import pytest

from fno.graph import relatedness as R


def _node(nid, title="", domain=None, details="", slug="", **extra):
    e = {"id": nid, "title": title, "slug": slug, "details": details}
    if domain is not None:
        e["domain"] = domain
    e.update(extra)
    return e


# --- AC1-HP: build writes a best-first map, zero-signal pairs absent ---

def test_build_map_scores_and_orders(tmp_path):
    entries = [
        _node("a", title="nightly backlog groomer relatedness", domain="code"),
        _node("b", title="nightly backlog groomer rank pass", domain="code"),
        _node("c", title="unrelated billing invoice export", domain="billing"),
    ]
    m = R.build_map(entries)
    # a and b share domain + many tokens -> related, both directions.
    assert any(r["id"] == "b" for r in m["a"])
    assert any(r["id"] == "a" for r in m["b"])
    # c shares nothing with a -> absent (zero-signal dropped).
    assert all(r["id"] != "c" for r in m["a"])
    # rows carry id/score/reason, best-first.
    top = m["a"][0]
    assert set(top) == {"id", "score", "reason"} and top["score"] > 0


def test_build_map_top_k_cap():
    # 6 mutually-related nodes; K=2 keeps only the 2 best per node.
    entries = [_node(str(i), title="shared groomer relatedness token", domain="code") for i in range(6)]
    m = R.build_map(entries, k=2)
    assert all(len(v) <= 2 for v in m.values())


def test_build_map_never_mutates_entries():
    entries = [_node("a", title="x groomer", domain="code"), _node("b", title="x groomer", domain="code")]
    snapshot = json.dumps(entries, sort_keys=True)
    R.build_map(entries)
    assert json.dumps(entries, sort_keys=True) == snapshot


def test_build_map_skips_malformed_rows():
    entries = [{"title": "no id here"}, _node("b", title="valid groomer", domain="code")]
    m = R.build_map(entries)  # must not raise
    assert "b" in m


# --- AC4-EDGE: empty / single-node graph is a clean no-op ---

def test_build_map_empty_graph():
    assert R.build_map([]) == {}


def test_build_map_single_node():
    m = R.build_map([_node("solo", title="lonely node", domain="code")])
    assert m == {"solo": []}


# --- AC3-ERR: no-map vs no-edges are distinguishable ---

def test_get_related_no_map_raises(tmp_path):
    with pytest.raises(R.NoMapError):
        R.get_related(tmp_path / "missing.json", "a")


def test_get_related_no_edges_returns_empty(tmp_path):
    p = tmp_path / "relatedness.json"
    R.write_map(p, {"a": []})
    assert R.get_related(p, "a") == []
    # unknown id in a present map is also just "no edges".
    assert R.get_related(p, "zzz") == []


def test_get_related_unreadable_map_raises(tmp_path):
    p = tmp_path / "relatedness.json"
    p.write_text("{ not json")
    with pytest.raises(R.NoMapError):
        R.get_related(p, "a")


def test_get_related_respects_k(tmp_path):
    p = tmp_path / "relatedness.json"
    R.write_map(p, {"a": [{"id": "b", "score": 0.9, "reason": ""}, {"id": "c", "score": 0.5, "reason": ""}]})
    assert [r["id"] for r in R.get_related(p, "a", k=1)] == ["b"]


# --- atomic write round-trips ---

def test_write_map_round_trip(tmp_path):
    p = tmp_path / "sub" / "relatedness.json"  # parent auto-created
    mapping = {"a": [{"id": "b", "score": 0.42, "reason": "shared domain 'code'"}]}
    R.write_map(p, mapping)
    assert json.loads(p.read_text()) == mapping


# --- AC8-EDGE: archive (consumed as-is by the nightly wrapper) never touches a
# referenced or too-recent terminal node. Guards the contract the composition
# relies on. ---

def test_archive_holds_back_referenced_and_recent():
    from fno.graph.archive import partition_for_archive

    now = datetime.now(timezone.utc)
    old = "2020-01-01T00:00:00+00:00"
    entries = [
        {"id": "done-old-ref", "completed_at": old},          # old but referenced
        {"id": "done-old-free", "completed_at": old},          # old, unreferenced -> archives
        {"id": "done-young", "completed_at": now.isoformat()}, # too recent
        {"id": "open", "blocked_by": ["done-old-ref"]},        # open node referencing the first
    ]
    to_archive, _, skipped = partition_for_archive(entries, older_than_days=30, now=now)
    ids = {e["id"] for e in to_archive}
    assert ids == {"done-old-free"}
    skip_reasons = {e["id"]: e["_skip"] for e in skipped}
    assert skip_reasons["done-old-ref"] == "referenced-by-open-node"
    assert skip_reasons["done-young"] == "too-recent"
