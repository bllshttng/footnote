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


def test_malformed_rows_are_evidence_for_one_caller_and_noise_for_the_rest(
    tmp_path: Path,
) -> None:
    """Exactly one reader keeps a junk row; everything else drops it.

    `agents/discover.py::_reachable_from_graph` COUNTS non-dict rows via
    `load_graph` to report "graph unreadable" instead of "token names nothing"
    -- its own comment says reporting empty there would drop the mail. That is
    the sole caller treating a junk row as evidence, so it is the sole caller
    passing `keep_malformed=True`.

    Every other consumer indexes what it gets back, so the default drops.
    """
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": [42, None, {"id": "x-0004"}]}), encoding="utf-8")

    raw = load_graph(p)
    assert any(not isinstance(e, dict) for e in raw), (
        "load_graph removed the malformed rows corruption detection depends on"
    )
    assert [e["id"] for e in raw if isinstance(e, dict)] == ["x-0004"]

    from fno.graph.store import read_graph_strict

    for reader in (read_graph, read_graph_strict, read_graph_nodes):
        rows = reader(p)
        assert all(isinstance(e, dict) for e in rows), (
            f"{reader.__name__} handed a non-dict row to a caller that indexes dicts"
        )
        assert [e["id"] for e in rows] == ["x-0004"]


def test_ordinary_read_commands_survive_a_malformed_row(tmp_path: Path) -> None:
    """The shapes the review named: dict-indexing consumers must not raise.

    `read_graph`'s documented job is that `status` and `ready` do not crash on a
    wedged graph. These are the two access patterns cited as breaking.
    """
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": [42, {"id": "x-0004"}]}), encoding="utf-8")

    entries = read_graph(p)
    assert {e["id"]: e for e in entries}.keys() == {"x-0004"}   # cmd_tree's shape
    assert [e.get("id") for e in entries] == ["x-0004"]         # resolve_node's shape


def test_node_lookup_tolerates_the_rows_load_graph_preserves(tmp_path: Path) -> None:
    """`_find_node` is where every load_graph caller resolves, so it must guard.

    Preserving junk for the corruption detector puts those rows in front of the
    shared lookup helper; without the guard `fno backlog update --pr-number`
    raises AttributeError before the lock is even taken.
    """
    from fno.graph._intake import _find_node

    entries = load_graph(_write(tmp_path, [42, {"id": "x-0004", "title": "real"}]))
    assert _find_node(entries, "x-0004")["title"] == "real"
    assert _find_node(entries, "x-nope") is None


def _write(tmp_path: Path, entries: list) -> Path:
    p = tmp_path / "graph.json"
    p.write_text(json.dumps({"entries": entries}), encoding="utf-8")
    return p


def test_a_mutation_announces_what_it_drops_and_keeps_a_backup(tmp_path: Path) -> None:
    """The write path drops junk rows -- it must not do so silently.

    Keeping them here is not an option: ensure_slugs, recompute_statuses and
    canonicalize_entries all assume dicts, so a preserved row wedges every
    future write with no self-healing path (the only code that could rewrite the
    file is the code that crashes on it). Dropping is correct; dropping quietly
    is not.
    """
    from fno.graph.store import locked_mutate_graph

    p = _write(tmp_path, [{"id": "x-0005", "title": "real"}, 42])

    locked_mutate_graph(p, lambda entries: entries)   # must not raise

    on_disk = json.loads(p.read_text())["entries"]
    assert [e["id"] for e in on_disk] == ["x-0005"]
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
