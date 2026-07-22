"""US5 / AC2-FR: a graph read failure never degrades into wrong provenance.

resolve_provenance reads the graph to normalize a slug input to an id. Under the
sidecar two-write window that read can raise. The old `except Exception: pass`
left `node` as the slug and exported FNO_NODE=<slug>, which the origin-capture
side then drops as an unknown node -- landing source_node_id null and blaming a
bad node id for what was really a read failure. The read failure must instead
leave FNO_NODE absent (never a slug), while resolve_provenance still returns
normally so the spawn is never failed.
"""
from __future__ import annotations

import pytest

from fno.agents.mux_spawn import resolve_provenance


@pytest.fixture(autouse=True)
def _clear_ambient(monkeypatch):
    for var in ("FNO_NODE", "FNO_SLUG", "FNO_PLAN"):
        monkeypatch.delenv(var, raising=False)


def _make_load_graph_fail(monkeypatch):
    from fno.graph.load import GraphCorruptionError

    def _boom(*_a, **_k):
        raise GraphCorruptionError.__new__(GraphCorruptionError)  # any read failure

    monkeypatch.setattr("fno.graph.load.load_graph", _boom)


def test_read_failure_drops_slug_never_exports_it(monkeypatch):
    _make_load_graph_fail(monkeypatch)
    # A slug input that could not be normalized must NOT become FNO_NODE=<slug>.
    prov = resolve_provenance("the-origin-node")
    assert "FNO_NODE" not in prov
    assert "the-origin-node" not in prov.values()


def test_read_failure_keeps_a_resolved_id(monkeypatch):
    _make_load_graph_fail(monkeypatch)
    # An id input is already resolved; a failed slug-enrichment read keeps it.
    prov = resolve_provenance("x-aaaa")
    assert prov.get("FNO_NODE") == "x-aaaa"


def test_read_failure_drops_an_id_shaped_slug(monkeypatch):
    _make_load_graph_fail(monkeypatch)
    # A title-derived slug can pass the LIBERAL has_node_id_prefix (starts with
    # the id prefix) yet is not a strict well-formed id (non-hex suffix). It must
    # still be dropped on a read failure, not leaked into FNO_NODE.
    prov = resolve_provenance("x-marks-the-spot")
    assert "FNO_NODE" not in prov
    assert "x-marks-the-spot" not in prov.values()


def test_read_failure_swallow_never_raises_to_caller(monkeypatch):
    _make_load_graph_fail(monkeypatch)
    # resolve_provenance must return normally -- a read failure never fails the spawn.
    assert resolve_provenance("the-origin-node") == {}
    assert resolve_provenance("x-aaaa") == {"FNO_NODE": "x-aaaa"}


def test_read_failure_with_id_and_prefilled_slug_plan(monkeypatch):
    _make_load_graph_fail(monkeypatch)
    # slug+plan supplied and node is an id: no graph read is even needed, but
    # even if it fails the id survives and the supplied fields pass through.
    prov = resolve_provenance("x-aaaa", slug="s", plan="/p.md")
    assert prov == {"FNO_NODE": "x-aaaa", "FNO_SLUG": "s", "FNO_PLAN": "/p.md"}


def test_dropped_slug_is_traceable_under_fno_debug(monkeypatch, capsys):
    # The drop must not be entirely invisible: under FNO_DEBUG the read failure
    # that dropped the node is logged, so a missing-origin node is traceable.
    _make_load_graph_fail(monkeypatch)
    monkeypatch.setenv("FNO_DEBUG", "1")
    prov = resolve_provenance("the-origin-node")
    assert "FNO_NODE" not in prov
    err = capsys.readouterr().err
    assert "resolve_provenance" in err
    assert "the-origin-node" in err


def test_dropped_slug_is_silent_without_fno_debug(monkeypatch, capsys):
    _make_load_graph_fail(monkeypatch)
    monkeypatch.delenv("FNO_DEBUG", raising=False)
    resolve_provenance("the-origin-node")
    assert capsys.readouterr().err == ""
