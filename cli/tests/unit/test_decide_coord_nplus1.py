"""The coord-lifecycle N+1, and the parity that made fixing it safe.

``list_decisions`` reads the graph ONCE and hands it to ``_coord_lifecycle``,
which handed it to nothing: ``_derive_coord_expiry_ref`` ignored the entries it
was given and called ``_graph_entries(required=True)`` itself, once per coord
row. Measured 2026-09-03 on a 1098-row store with 664 coord rows: 32s for one
query, effectively all of it in ~540 strict re-reads of a 2400-node graph.
After threading the entries through, the same query answers in under a second
and its payload is byte-identical. A SessionStart nag cannot afford the former,
which is how this was found.

Every test here asserts a POSITIVE marker rather than a wall-clock time. A
timing assertion is flaky and, when it fails, does not say WHY it got slow.
"""
from __future__ import annotations

import pytest


def test_derive_coord_expiry_ref_reads_no_graph_when_entries_are_supplied(monkeypatch):
    """The N+1 guard: a supplied graph is used, never re-read."""
    from fno.decide import _derive_coord_expiry_ref

    def _explode(**kwargs):
        raise AssertionError(
            "_derive_coord_expiry_ref re-read the graph despite being handed "
            "one; this is the N+1 that cost 32s per query"
        )

    monkeypatch.setattr("fno.decide._graph_entries", _explode)

    entries = [{"id": "x-1234", "slug": "a-node", "status": "ready"}]
    assert _derive_coord_expiry_ref("x-1234", None, entries) == {
        "kind": "node",
        "node_id": "x-1234",
    }


def test_supplying_entries_answers_exactly_as_self_reading_did(monkeypatch):
    """Parity on the readable-graph path: same graph in, same closure key out."""
    from fno.decide import _derive_coord_expiry_ref

    entries = [{"id": "x-1234", "slug": "a-node", "status": "ready"}]
    monkeypatch.setattr("fno.decide._graph_entries", lambda **kwargs: list(entries))

    self_read = _derive_coord_expiry_ref("x-1234", None)
    supplied = _derive_coord_expiry_ref("x-1234", None, entries)
    assert supplied == self_read
    # Positive marker, not bare equality: two Nones would also be "equal".
    assert supplied == {"kind": "node", "node_id": "x-1234"}


def test_an_explicit_expiry_ref_short_circuits_before_any_graph_read(monkeypatch):
    """A row that already carries its own ref never reaches the graph at all."""
    from fno.decide import _derive_coord_expiry_ref

    def _explode(**kwargs):
        raise AssertionError("an explicit expiry_ref must not trigger a graph read")

    monkeypatch.setattr("fno.decide._graph_entries", _explode)

    ref = {"kind": "node", "node_id": "x-9999"}
    assert _derive_coord_expiry_ref("x-9999", ref) == ref


def test_unreadable_graph_differs_here_and_converges_at_the_lifecycle(monkeypatch):
    """The one path where the two differ, stated rather than glossed.

    Before: an unreadable graph raised inside this function, which returned
    None. After: the caller degrades to ``[]`` and passes it, ``_resolved_node``
    answers None on the empty list, and control falls through to
    ``_pr_expiry_ref``. A repository-scoped PR subject is the only input that
    tells the two apart, so it is the input used here.

    ``_coord_lifecycle`` then resolves that ref against the same empty entries,
    matches no node, and answers ``unscoped`` - which is exactly what the None
    produced. The VERDICT is unchanged; only the intermediate value moved.
    """
    from fno.decide import _coord_lifecycle, _derive_coord_expiry_ref

    def _raise(**kwargs):
        raise RuntimeError("graph unreadable")

    monkeypatch.setattr("fno.decide._graph_entries", _raise)

    subject = "bllshttng/footnote#1234"
    assert _derive_coord_expiry_ref(subject, None) is None
    assert _derive_coord_expiry_ref(subject, None, []) == {
        "kind": "pr",
        "repository": "bllshttng/footnote",
        "number": 1234,
    }

    # The claim that made the change safe: one frame up, both answer the same.
    lifecycle, evidence = _coord_lifecycle({"subject": subject}, [])
    assert lifecycle == "unscoped"
    assert evidence is None


@pytest.mark.parametrize("subject", ["", None])
def test_a_subjectless_row_reads_no_graph(monkeypatch, subject):
    """No subject proves no closure key, so there is nothing to look up."""
    from fno.decide import _derive_coord_expiry_ref

    def _explode(**kwargs):
        raise AssertionError("a subjectless row must not trigger a graph read")

    monkeypatch.setattr("fno.decide._graph_entries", _explode)

    assert _derive_coord_expiry_ref(subject, None) is None
