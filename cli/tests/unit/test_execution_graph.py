"""Execution-graph types + compiler (wave 4 of the context-graph epic).

Covers the seven canonical compile fixtures against stable serialized output
(one-node collapse, dependency chain, safe fan-out, file collision, unknown
liveness, verification edge, fan-in barrier) plus the strict-parse contract:
every type fails CLOSED on a schema defect and round-trips through to_dict.
"""
from __future__ import annotations

import pytest

from fno.graph.execution import (
    EXEC_GRAPH_VERSION,
    Barrier,
    Budget,
    ExecEdge,
    ExecGraph,
    ExecNode,
    MalformedGraphError,
    compile_graph,
)


def _entry(node_id: str, **kw):
    e = {"id": node_id, "title": f"node {node_id}", "project": "fno"}
    e.update(kw)
    return e


# ── compile fixtures ─────────────────────────────────────────────────────────


def test_one_node_collapses_to_serial():
    g = compile_graph("a", [_entry("a")])
    assert g.collapsed is True
    assert len(g.nodes) == 1
    assert g.edges == ()
    assert g.barriers == ()
    assert g.nodes[0].justification == "specialized"


def test_dependency_chain_emits_ordering_edges():
    entries = [_entry("a"), _entry("b", blocked_by=["a"]), _entry("c", blocked_by=["b"])]
    g = compile_graph("c", entries)
    assert g.collapsed is False
    kinds = {(e.src, e.dst, e.kind) for e in g.edges}
    assert kinds == {("a", "b", "ordering"), ("b", "c", "ordering")}
    # a single predecessor each -> no fan-in barrier
    assert g.barriers == ()


def test_safe_fan_out_marks_parallel_siblings():
    # a blocks both b and c; b and c are independent and run in parallel.
    entries = [_entry("a"), _entry("b", blocked_by=["a"]), _entry("c", blocked_by=["a"])]
    g = compile_graph("a", entries)
    edges = {(e.src, e.dst, e.kind) for e in g.edges}
    assert edges == {("a", "b", "ordering"), ("a", "c", "ordering")}
    just = {n.node_id: n.justification for n in g.nodes}
    assert just["b"] == "parallelism"
    assert just["c"] == "parallelism"
    assert g.barriers == ()  # each dependent has one predecessor


def test_file_collision_emits_resource_edge():
    entries = [_entry("x", owns_files=["shared.py"]), _entry("y", owns_files=["shared.py"])]
    g = compile_graph("x", entries)
    resource = [e for e in g.edges if e.kind == "resource"]
    assert len(resource) == 1
    assert resource[0].src == "x" and resource[0].dst == "y"
    assert "shared.py" in resource[0].invariant
    just = {n.node_id: n.justification for n in g.nodes}
    assert just["x"] == "resource" and just["y"] == "resource"


def test_unknown_liveness_is_carried_on_the_node():
    entries = [_entry("a", locked_by="sess-1"), _entry("b", blocked_by=["a"])]
    # emulate the CLI enrichment: a held claim with unprobed liveness
    entries[0]["claim_holder"] = "sess-1"
    entries[0]["liveness"] = "unknown"
    g = compile_graph("b", entries)
    node_a = next(n for n in g.nodes if n.node_id == "a")
    assert node_a.liveness == "unknown"
    assert node_a.claim_key == "sess-1"


def test_verification_edge_from_node_to_its_verifier():
    entries = [_entry("impl", verifier="check"), _entry("check")]
    g = compile_graph("impl", entries)
    verif = [e for e in g.edges if e.kind == "verification"]
    assert len(verif) == 1
    assert verif[0].src == "impl" and verif[0].dst == "check"
    just = {n.node_id: n.justification for n in g.nodes}
    assert just["check"] == "verifier"


def test_fan_in_barrier_when_two_predecessors():
    entries = [_entry("d1"), _entry("d2"), _entry("t", blocked_by=["d1", "d2"])]
    g = compile_graph("t", entries)
    assert len(g.barriers) == 1
    b = g.barriers[0]
    assert b.at == "t"
    assert b.expected == ("d1", "d2")
    just = {n.node_id: n.justification for n in g.nodes}
    assert just["t"] == "branching"


def test_resource_edge_skipped_when_ordering_serializes():
    # a depends on b and both own the same file: ordering already serializes
    # them, so a contradicting resource edge (which would form a cycle) is NOT
    # emitted.
    entries = [_entry("b", owns_files=["shared.py"]),
               _entry("a", blocked_by=["b"], owns_files=["shared.py"])]
    g = compile_graph("a", entries)
    assert [e.kind for e in g.edges] == ["ordering"]
    assert g.edges[0].src == "b" and g.edges[0].dst == "a"
    # no edge pair points both ways
    pairs = {(e.src, e.dst) for e in g.edges}
    assert not any((dst, src) in pairs for src, dst in pairs)


def test_data_edge_from_evidence_producer():
    entries = [
        _entry("prod", produces_evidence=["receipt"]),
        _entry("cons", requires_evidence=["receipt"]),
    ]
    g = compile_graph("cons", entries)
    data = [e for e in g.edges if e.kind == "data"]
    assert len(data) == 1
    assert data[0].src == "prod" and data[0].dst == "cons"


# ── serialization round-trip + snapshot stability ────────────────────────────


def test_to_dict_is_sorted_and_stable():
    entries = [_entry("d2"), _entry("d1"), _entry("t", blocked_by=["d2", "d1"])]
    g = compile_graph("t", entries)
    d = g.to_dict()
    # nodes sorted by id, barriers' expected sorted -> identical across runs
    assert [n["node_id"] for n in d["nodes"]] == ["d1", "d2", "t"]
    assert d["barriers"][0]["expected"] == ["d1", "d2"]
    assert d["version"] == EXEC_GRAPH_VERSION


def test_round_trip_through_strict_parse():
    entries = [_entry("d1"), _entry("d2"), _entry("t", blocked_by=["d1", "d2"])]
    g = compile_graph("t", entries)
    reparsed = ExecGraph.from_dict(g.to_dict())
    assert reparsed.to_dict() == g.to_dict()


# ── strict-parse: fail CLOSED on every schema defect ─────────────────────────


def test_bad_edge_kind_rejected():
    with pytest.raises(MalformedGraphError):
        ExecEdge("a", "b", "sideways", "inv")


def test_self_loop_edge_rejected():
    with pytest.raises(MalformedGraphError):
        ExecEdge("a", "a", "ordering", "inv")


def test_edge_without_invariant_rejected():
    with pytest.raises(MalformedGraphError):
        ExecEdge("a", "b", "data", "   ")


def test_bad_justification_rejected():
    with pytest.raises(MalformedGraphError):
        ExecNode(node_id="a", role="builder", justification="vibes", objective="o", repo="r")


def test_budget_rejects_bool_as_int():
    with pytest.raises(MalformedGraphError):
        Budget(max_iterations=True)  # bool must not pose as 1


def test_barrier_without_expected_rejected():
    with pytest.raises(MalformedGraphError):
        Barrier(at="t", expected=())


def test_from_dict_version_mismatch_rejected():
    with pytest.raises(MalformedGraphError):
        ExecGraph.from_dict({"version": 999, "root": "a", "nodes": []})


def test_from_dict_edge_referencing_unknown_node_rejected():
    d = {
        "version": EXEC_GRAPH_VERSION,
        "root": "a",
        "nodes": [ExecNode(node_id="a", role="builder", justification="specialized",
                           objective="o", repo="r").to_dict()],
        "edges": [{"src": "a", "dst": "ghost", "kind": "ordering", "invariant": "x"}],
    }
    with pytest.raises(MalformedGraphError):
        ExecGraph.from_dict(d)


def test_compile_empty_root_raises():
    with pytest.raises(MalformedGraphError):
        compile_graph("", [])


def test_compile_root_absent_falls_back_to_serial():
    # root not in the provided set -> collapsed serial, never an exception
    g = compile_graph("missing", [_entry("other")])
    assert g.collapsed is True
    assert g.root == "missing"


def test_relevant_scope_is_order_independent():
    # chain root(c) <- b <- a; scope must be {a,b,c} regardless of dict order.
    from fno.graph.cli import _relevant_exec_scope

    nodes = [_entry("a"), _entry("b", blocked_by=["a"]), _entry("c", blocked_by=["b"])]
    forward = {e["id"]: e for e in nodes}
    reverse = {e["id"]: e for e in reversed(nodes)}
    assert _relevant_exec_scope("c", forward) == {"a", "b", "c"}
    assert _relevant_exec_scope("c", forward) == _relevant_exec_scope("c", reverse)


def test_relevant_scope_pulls_verifier_and_evidence():
    from fno.graph.cli import _relevant_exec_scope

    by_id = {
        "impl": _entry("impl", verifier="check", requires_evidence=["r"]),
        "check": _entry("check"),
        "prod": _entry("prod", produces_evidence=["r"]),
        "unrelated": _entry("unrelated"),
    }
    scope = _relevant_exec_scope("impl", by_id)
    assert scope == {"impl", "check", "prod"}  # unrelated stays out
