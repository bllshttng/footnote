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


def test_junk_rows_are_dropped_rather_than_crashing(tmp_path: Path) -> None:
    """A non-dict entry is unusable in every path; it must not raise.

    `read_graph`'s contract is to swallow corruption so a wedged graph never
    crashes a user's terminal. Before the guard moved into the shared defaults
    pass, a scalar row reached it and surfaced as a bare AttributeError.
    """
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": [42, None, {"id": "x-0004"}]}), encoding="utf-8")

    for reader in (read_graph, load_graph, read_graph_nodes):
        rows = reader(p)
        assert [e["id"] for e in rows] == ["x-0004"], f"{reader.__name__} mishandled junk"


def test_missing_graph_is_empty_not_an_error(tmp_path: Path) -> None:
    """An absent graph is empty for every reader -- not corruption, not a raise."""
    absent = tmp_path / "nope.json"
    for reader in (read_graph, load_graph, read_graph_nodes):
        assert reader(absent) == [], f"{reader.__name__} did not degrade to []"


def test_write_path_announces_dropped_rows_and_keeps_a_backup(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """Dropping a junk row is silent on reads and LOUD on the write path.

    The defaults pass drops non-dict rows so no reader crashes on them, and
    `locked_mutate_graph` feeds that same filtered list to `_write_json` -- so on
    that path the drop is persisted. A `backlog update` must not delete a row
    from the user's graph without saying so.
    """
    from fno.graph.store import locked_mutate_graph

    p = tmp_path / "graph.json"
    p.write_text(
        json.dumps({"entries": [{"id": "x-0005", "title": "real"}, 42]}), encoding="utf-8"
    )

    locked_mutate_graph(p, lambda entries: entries)

    err = capsys.readouterr().err
    assert "malformed graph" in err, f"write path dropped a row silently: {err!r}"

    surviving = [e["id"] for e in read_graph(p)]
    assert surviving == ["x-0005"]
    assert list(tmp_path.glob("graph.json.bak*")), "no backup left to recover the dropped row"


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
