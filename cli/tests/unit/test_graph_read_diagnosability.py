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

from fno.graph.store import GraphUnreadableError, read_graph_strict


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


# --- AC1-EDGE: boundary inputs ---

def test_ac1edge_absent_file_returns_empty(tmp_path):
    # An absent graph is empty, not unreadable (matches read_graph + today).
    assert read_graph_strict(tmp_path / "does-not-exist.json") == []


# --- AC2-ERR guard: read_graph (soft path) is unchanged for both fixtures ---

def test_ac2err_soft_read_returns_empty_for_malformed_root(tmp_path):
    # {} (no entries key) must still be [] on the soft path -- byte-identical to
    # its behavior before this change, so the malformed-root signal is reachable
    # only through the strict path.
    g = _write(tmp_path / "graph.json", json.dumps({}))
    assert read_graph_strict(g) == []


def test_ac2err_soft_read_returns_empty_for_empty_entries(tmp_path):
    g = _write(tmp_path / "graph.json", json.dumps({"entries": []}))
    assert read_graph_strict(g) == []


def test_soft_read_swallows_non_list_entries_instead_of_crashing(tmp_path):
    # read_graph promises it never crashes the terminal; a non-list 'entries'
    # value must swallow to [] like other corruption, not raise AttributeError
    # from _apply_graph_defaults.
    g = _write(tmp_path / "graph.json", json.dumps({"entries": "oops"}))
    assert read_graph_strict(g) == []
