"""All graph readers speak one migrated vocabulary.

`load_graph` and `read_graph_nodes` (scoreboard) read the graph.json seed
through the keeper's defaults pass; `read_graph_strict` serves typed rows from
the store. The store owns import policy now: a seed row the typed model cannot
represent is CARRIED verbatim (nodes_raw) and every reader applies the same
defaults pass over it, so both legs answer one vocabulary.

The parity assertions below are the seam: they fail if any reader grows its own
migration logic again.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.graph.load import load_graph
from fno.graph.store import read_graph_strict
from fno.scoreboard.fold import read_graph_nodes

# One legacy row per shape the migration has to handle, plus a current-vocabulary
# row that must survive untouched.
_LEGACY_GRAPH = {
    "entries": [
        {"id": "x-0001", "_status": "claimed", "priority": "high"},
        {"id": "x-0002", "status": "claimed"},
        {"id": "x-0003", "status": "ready", "priority": "p1"},
    ]
}


@pytest.fixture()
def graph(tmp_path: Path) -> Path:
    p = tmp_path / "graph.json"
    p.write_text(json.dumps(_LEGACY_GRAPH), encoding="utf-8")
    return p


def _by_id(entries: list[dict]) -> dict[str, dict]:
    return {e["id"]: e for e in entries}


def test_load_graph_applies_the_status_value_rename(graph: Path) -> None:
    """The specific bug: `claimed` on disk must read as `in_progress`.

    The VALUE rename survives on both legs. The pre-rename `_status` KEY
    spelling is adopted by the shared defaults pass, then renamed the same
    way, so a legacy row reads its claimed value migrated, not defaulted.
    """
    for rows in (_by_id(load_graph(graph)), _by_id(read_graph_strict(graph))):
        assert rows["x-0002"]["status"] == "in_progress"
        assert rows["x-0003"]["status"] == "ready"
    assert _by_id(load_graph(graph))["x-0001"]["status"] == "in_progress"


def test_load_graph_applies_the_priority_migration(graph: Path) -> None:
    """`load_graph` also skipped PRIORITY_MIGRATION, so board sorts disagreed."""
    rows = _by_id(load_graph(graph))
    assert rows["x-0001"]["priority"] == "p1"
    assert rows["x-0003"]["priority"] == "p1"


def test_every_reader_returns_identical_entries(graph: Path) -> None:
    """The seam: three readers, one migrated vocabulary.

    Full row equality died with the json leg: the typed store row carries
    fields the seed pass never minted (persisted_status, contained_in) and the
    scoreboard row omits write-path keys (slug, title). What must still agree
    is the vocabulary consumers branch on: status/priority for every row the
    typed model can represent, on the same defaulted keys (domain, blocked_by).

    x-0001 is the one sanctioned split: `_status` is not a modeled key, so the
    store-view readers (strict, load_graph) drop it and answer the default,
    while the scoreboard's seed view still folds the legacy spelling. A live
    mirror never carries `_status` - the keeper publishes normalized keys - so
    the split is pinned here as known rather than left to look like drift.
    """
    canonical = _by_id(read_graph_strict(graph))
    for nid in ("x-0002", "x-0003"):
        for reader in (load_graph, read_graph_nodes):
            row = _by_id(reader(graph))[nid]
            for key in ("status", "priority", "domain", "blocked_by"):
                assert row[key] == canonical[nid][key], (
                    f"{reader.__name__} disagrees with the store on {key} of {nid}"
                )
    # x-0001's `_status` is adopted by the shared defaults pass on every
    # leg now: one vocabulary means one answer for the legacy spelling.
    assert _by_id(load_graph(graph))["x-0001"]["status"] == "in_progress"
    assert _by_id(read_graph_nodes(graph))["x-0001"]["status"] == "in_progress"


def test_ordinary_read_commands_survive_a_malformed_row(tmp_path: Path) -> None:
    """The shapes the review named: dict-indexing consumers must not raise.

    `read_graph`'s documented job is that `status` and `ready` do not crash on a
    wedged graph. These are the two access patterns cited as breaking.
    """
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": [42, {"id": "x-0004"}]}), encoding="utf-8")

    entries = read_graph_strict(p)
    assert {e["id"]: e for e in entries}.keys() == {"x-0004"}   # cmd_tree's shape
    assert [e.get("id") for e in entries] == ["x-0004"]         # resolve_node's shape


def test_only_the_evidence_caller_ever_sees_a_malformed_row(tmp_path: Path) -> None:
    """The reason no other consumer needs an isinstance guard.

    An earlier attempt had `load_graph` preserve junk for everyone and guarded
    one consumer (`_find_node`). That is a guard on one of N reachable paths:
    `fuzzy.resolve_node`, `dispatch._lookup_node`, and `target_cli`'s slug
    resolver all scan the same list and would still crash or silently degrade --
    the slug path in particular fails toward skipping the node claim, which is
    the duplicate-worker bug that resolver exists to prevent.

    Making preservation opt-in removes the hazard instead of guarding it, so
    this pins the property the guards are no longer needed for.
    """
    from fno.graph._intake import _find_node
    from fno.graph.fuzzy import resolve_node

    p = _write(tmp_path, [42, {"id": "x-0004", "slug": "real-node", "title": "real"}])
    entries = load_graph(p)   # the default every ordinary caller takes

    assert _find_node(entries, "x-0004")["title"] == "real"
    assert resolve_node("real-node", entries).kind == "exact"


def _write(tmp_path: Path, entries: list) -> Path:
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": entries}), encoding="utf-8")
    return p


def test_scoreboard_reader_stays_silent_and_writes_nothing_on_corruption(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """The optional survival signal must not warn or leave a .bak behind.

    Sharing the migration pass must not mean sharing `read_graph`'s corruption
    POLICY. `read_graph` copies a .bak and warns on stderr before degrading to
    [], which is right for a command whose job is the graph and wrong here: a
    read-only scoreboard must not write files, and the warning lands in the `-J`
    stream and makes the output unparseable as JSON.
    """
    p = tmp_path / "graph.json"
    p.write_text("null", encoding="utf-8")  # corrupt-but-valid JSON

    assert read_graph_nodes(p) == []
    captured = capsys.readouterr()
    assert captured.out == "" and captured.err == "", (
        f"scoreboard reader emitted output on a corrupt graph: "
        f"{captured.out!r} / {captured.err!r}"
    )
    assert list(tmp_path.glob("*.bak*")) == [], "read-only reader left a backup file"


def test_unhashable_field_values_do_not_crash_the_readers(tmp_path: Path) -> None:
    """A dict row whose `priority`/`status` is unhashable must not raise.

    The migration hashes the value, so a hand-mangled `"priority": []` once
    raised TypeError out of the shared pass, crashing readers documented as
    never-fatal. No reader raises now; the unrepresentable row rides the
    raw carry verbatim (values untouched) while its sibling still migrates.
    """
    p = _write(tmp_path, [
        {"id": "x-0006", "priority": [], "status": {"nope": 1}},
        {"id": "x-0007", "priority": "p1", "status": "claimed"},
    ])
    for reader in (read_graph_strict, load_graph):
        rows = _by_id(reader(p))
        assert set(rows) == {"x-0006", "x-0007"}, f"{reader.__name__} lost a carried row"
        assert rows["x-0007"]["status"] == "in_progress"   # sibling still migrates
    rows = _by_id(read_graph_nodes(p))
    assert rows["x-0006"]["priority"] == []      # unmigratable, left alone
    assert rows["x-0007"]["status"] == "in_progress"


def test_strict_reader_reports_unreadable_rather_than_absent(tmp_path: Path) -> None:
    """`read_graph_strict` must raise GraphUnreadableError for ANY unreadable graph.

    Callers branch on that type to tell a wedged graph from a missing node, and
    a `read_text()` outside the guard let a directory, a permission error, or
    non-UTF-8 bytes escape as OSError/UnicodeDecodeError -- past the caller's
    handler and out as a generic exit 1, the code meaning "read cleanly, node
    absent".
    """
    from fno.graph.store import GraphUnreadableError, read_graph_strict

    a_directory = tmp_path / "graph.json"
    a_directory.mkdir()
    with pytest.raises(GraphUnreadableError):
        read_graph_strict(a_directory)

    not_utf8 = tmp_path / "binary.json"
    not_utf8.write_bytes(b'{"entries": [\xff\xfe]}')
    with pytest.raises(GraphUnreadableError):
        read_graph_strict(not_utf8)
