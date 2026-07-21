"""US1 + US3: the strict graph reader distinguishes the read-failure states.

The defect this covers: ``read_graph`` swallows every failure to ``[]``, so a
resolution caller cannot tell an unreadable graph from a genuinely absent node
(the duplicate-filing class). ``read_graph_strict`` surfaces the difference while
leaving ``read_graph``'s soft contract untouched for the display commands.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.graph.store import (
    GraphMalformedRootError,
    GraphUnreadableError,
    read_graph,
    read_graph_strict,
)


def _write(p: Path, text: str) -> Path:
    p.write_text(text)
    return p


# --- AC1-HP: the strict reader distinguishes populated / empty / invalid ---

def test_ac1hp_populated_graph_returns_entries(tmp_path):
    g = _write(tmp_path / "graph.json", json.dumps({"entries": [{"id": "x-aaaa"}]}))
    entries = read_graph_strict(g)
    assert [e["id"] for e in entries] == ["x-aaaa"]


def test_ac1hp_empty_graph_returns_empty_quietly(tmp_path):
    g = _write(tmp_path / "graph.json", json.dumps({"entries": []}))
    assert read_graph_strict(g) == []


def test_ac1hp_invalid_json_raises_not_returns_list(tmp_path):
    g = _write(tmp_path / "graph.json", "{ this is not json")
    with pytest.raises(GraphUnreadableError):
        read_graph_strict(g)


# --- AC1-ERR: an unreadable graph is not an absent node ---

def test_ac1err_unreadable_names_the_path_and_not_absent(tmp_path):
    g = _write(tmp_path / "graph.json", "{ this is not json")
    with pytest.raises(GraphUnreadableError) as exc:
        read_graph_strict(g)
    assert str(g) in str(exc.value)
    assert "No node matching" not in str(exc.value)


# --- AC3-ERR: malformed root distinguishable from an empty graph ---

def test_ac3err_empty_graph_is_quiet(tmp_path):
    g = _write(tmp_path / "graph.json", json.dumps({"entries": []}))
    # empty graph: returns [], never raises
    assert read_graph_strict(g) == []


def test_ac3err_malformed_root_raises_distinct_signal(tmp_path):
    g = _write(tmp_path / "graph.json", json.dumps({}))
    with pytest.raises(GraphMalformedRootError) as exc:
        read_graph_strict(g)
    assert "entries" in str(exc.value)


def test_ac3err_malformed_root_is_a_subclass_of_unreadable(tmp_path):
    # A caller that only needs "unreadable vs absent" catches the base type;
    # a caller that wants the finer distinction catches the subclass.
    g = _write(tmp_path / "graph.json", json.dumps({}))
    with pytest.raises(GraphUnreadableError):
        read_graph_strict(g)
    assert issubclass(GraphMalformedRootError, GraphUnreadableError)


# --- AC1-EDGE: boundary inputs ---

def test_ac1edge_absent_file_returns_empty(tmp_path):
    # An absent graph is empty, not unreadable (matches read_graph + today).
    assert read_graph_strict(tmp_path / "does-not-exist.json") == []


def test_ac1edge_zero_byte_file_is_unreadable_not_empty(tmp_path):
    g = _write(tmp_path / "graph.json", "")
    with pytest.raises(GraphUnreadableError):
        read_graph_strict(g)


def test_ac1edge_bare_list_root_is_unreadable(tmp_path):
    g = _write(tmp_path / "graph.json", json.dumps([{"id": "x-aaaa"}]))
    with pytest.raises(GraphUnreadableError):
        read_graph_strict(g)


def test_ac1edge_no_bak_written_for_a_file_that_parsed(tmp_path):
    # A bare-list root parses fine as JSON; the strict path must NOT write a .bak
    # for a file it did not fail to parse.
    g = _write(tmp_path / "graph.json", json.dumps([{"id": "x-aaaa"}]))
    with pytest.raises(GraphUnreadableError):
        read_graph_strict(g)
    assert list(tmp_path.glob("*.bak*")) == []
    assert not (tmp_path / "graph.json.bak").exists()


# --- AC2-ERR guard: read_graph (soft path) is unchanged for both fixtures ---

def test_ac2err_soft_read_swallows_invalid_json_to_empty(tmp_path):
    g = _write(tmp_path / "graph.json", "{ not json")
    assert read_graph(g) == []


def test_ac2err_soft_read_returns_empty_for_malformed_root(tmp_path):
    # {} (no entries key) must still be [] on the soft path -- byte-identical to
    # its behavior before this change, so the malformed-root signal is reachable
    # only through the strict path.
    g = _write(tmp_path / "graph.json", json.dumps({}))
    assert read_graph(g) == []


def test_ac2err_soft_read_returns_empty_for_empty_entries(tmp_path):
    g = _write(tmp_path / "graph.json", json.dumps({"entries": []}))
    assert read_graph(g) == []


def test_soft_read_swallows_non_list_entries_instead_of_crashing(tmp_path):
    # read_graph promises it never crashes the terminal; a non-list 'entries'
    # value must swallow to [] like other corruption, not raise AttributeError
    # from _apply_graph_defaults.
    g = _write(tmp_path / "graph.json", json.dumps({"entries": "oops"}))
    assert read_graph(g) == []


def test_strict_read_raises_on_non_list_entries_value(tmp_path):
    g = _write(tmp_path / "graph.json", json.dumps({"entries": {"not": "a list"}}))
    with pytest.raises(GraphUnreadableError):
        read_graph_strict(g)
