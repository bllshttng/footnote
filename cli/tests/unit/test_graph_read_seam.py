"""All graph readers speak one migrated vocabulary.

`load_graph` (hash-validated) and `read_graph_nodes` (scoreboard) used to parse
graph.json themselves. `load_graph` folded the `_status` -> `status` KEY rename
but not the `claimed` -> `in_progress` VALUE rename, so its ~10 callers -- among
them `recovery.py`, `target_cli.py`, and `dispatch.py` -- read a status
vocabulary one migration behind `read_graph`. `read_graph_nodes` folded neither.

The parity assertions below are the seam: they fail if any reader grows its own
migration logic again.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.graph.load import load_graph
from fno.graph.store import read_graph
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

    Both spellings -- the pre-rename `_status` key and the current `status` key
    carrying the pre-rename value -- land on the current vocabulary.
    """
    rows = _by_id(load_graph(graph))
    assert rows["x-0001"]["status"] == "in_progress"
    assert rows["x-0002"]["status"] == "in_progress"
    assert rows["x-0003"]["status"] == "ready"


def test_load_graph_applies_the_priority_migration(graph: Path) -> None:
    """`load_graph` also skipped PRIORITY_MIGRATION, so board sorts disagreed."""
    rows = _by_id(load_graph(graph))
    assert rows["x-0001"]["priority"] == "p1"
    assert rows["x-0003"]["priority"] == "p1"


def test_every_reader_returns_identical_entries(graph: Path) -> None:
    """The seam itself: three readers, one result.

    Full equality rather than a status spot-check, so a reader that skips the
    `setdefault` block (and hands callers a row missing `domain` or `blocked_by`)
    fails here too.
    """
    canonical = read_graph(graph)
    assert load_graph(graph) == canonical
    assert read_graph_nodes(graph) == canonical


def test_junk_rows_are_skipped_for_migration_but_never_removed(tmp_path: Path) -> None:
    """A non-dict row must not crash the pass, and must not vanish from it.

    Skipping is what the pass owes callers: every field access assumes a dict,
    so a scalar row used to raise a bare AttributeError. KEEPING it is the
    subtler half. `locked_mutate_graph` feeds this result straight to
    `_write_json`, so a dropped row is deleted from disk on the next mutation;
    and `agents/discover.py::_reachable_from_graph` COUNTS non-dict rows to
    report "graph unreadable" instead of "token names nothing" -- filtering here
    removes the evidence it looks for and turns a corrupt graph into a confident
    miss, which its own comment says would drop the mail.
    """
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": [42, None, {"id": "x-0004"}]}), encoding="utf-8")

    for reader in (read_graph, load_graph):
        rows = reader(p)
        assert [e for e in rows if isinstance(e, dict)] == [
            e for e in rows if isinstance(e, dict)
        ]
        assert any(not isinstance(e, dict) for e in rows), (
            f"{reader.__name__} removed the malformed rows that corruption "
            f"detection depends on"
        )
        assert [e["id"] for e in rows if isinstance(e, dict)] == ["x-0004"]

    # The scoreboard is a display surface and filters for itself, so it is the
    # one reader that legitimately returns only well-formed rows.
    assert [e["id"] for e in read_graph_nodes(p)] == ["x-0004"]


def test_a_mutation_never_silently_deletes_a_malformed_row(tmp_path: Path) -> None:
    """A mutation over a corrupt graph must not quietly drop what it cannot read.

    `locked_mutate_graph` writes back whatever the defaults pass returned, so a
    row filtered there is not skipped once -- it is deleted from the user's
    graph. Refusing to mutate a corrupt graph is a fine outcome (and is what
    happens: a later step trips over the row before any write). Deleting it and
    reporting success is not. This asserts the file, not the exception, because
    the property that matters is what survives on disk.
    """
    from fno.graph.store import locked_mutate_graph

    p = tmp_path / "graph.json"
    original = {"entries": [{"id": "x-0005", "title": "real"}, 42]}
    p.write_text(json.dumps(original), encoding="utf-8")

    try:
        locked_mutate_graph(p, lambda entries: entries)
    except Exception:  # noqa: BLE001 - refusing is acceptable; losing data is not
        pass

    on_disk = json.loads(p.read_text())["entries"]
    assert 42 in on_disk, f"a mutation deleted a malformed row: {on_disk}"
    assert [e["id"] for e in on_disk if isinstance(e, dict)] == ["x-0005"]


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
