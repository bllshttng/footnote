"""`fno agents king drain` answers repeat fires from an identity-keyed cache.

The stop gate shells the drain on every fire, and each fire re-paid a
full-graph read that, under fleet load, outruns the gate's own read budget -
a king whose scope is delivered was blocked by a slow reader. The contract
pinned here: the drain returns inside STOPGATE_READ_TIMEOUT with the budget
read from the gate's own constant (never hardcoded), a scope with genuinely
undelivered nodes still reports its non-zero count after the speedup (a fast
always-zero reader must fail these tests, not pass), and the second fire on
an unchanged graph answers without touching the store at all.
"""

from __future__ import annotations

import json
import re
import time
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir

REPO_ROOT = Path(__file__).resolve().parents[3]
READ_BOUNDS = REPO_ROOT / "crates/fno-agents/src/loopcheck/read_bounds.rs"

SCOPE = "epic-fix"
FILLER = 2400
CHILDREN = 40
DONE_CHILDREN = 31
# The scope epic itself is a non-terminal row in its own scope, so a scope
# with 9 open children reads 10 undelivered, never 9.
UNDELIVERED_COUNT = CHILDREN - DONE_CHILDREN + 1


def _stopgate_read_timeout() -> int:
    """The gate's own constant, parsed from its source. A missing or
    unreadable constant FAILS the budget assert rather than skipping it:
    an absent ruler is not a pass."""
    text = READ_BOUNDS.read_text(encoding="utf-8")
    match = re.search(r"STOPGATE_READ_TIMEOUT[^;]*from_secs\((\d+)\)", text)
    assert match, f"STOPGATE_READ_TIMEOUT not parseable in {READ_BOUNDS}"
    return int(match.group(1))


@pytest.fixture
def graph(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    from fno.graph.store import _worker_binary

    if _worker_binary() is None:
        pytest.skip("no fno-agents-worker binary; the real keeper read path is the subject")
    from fno import paths

    graph_path = paths.graph_json()
    _write_graph(graph_path, done_children=DONE_CHILDREN)
    return graph_path


def _entry(node_id: str, **fields) -> dict:
    return {"id": node_id, **fields}


def _ident(graph: Path) -> tuple:
    from fno.king import drain_cache

    ident = drain_cache.graph_ident(graph)
    assert ident is not None
    return ident


def _write_graph(path: Path, done_children: int, done_epic: bool = False) -> None:
    entries = [
        _entry(f"filler-{i}", type="feature", status="intake", project="web")
        for i in range(FILLER)
    ]
    entries.append(
        _entry(
            SCOPE,
            type="epic",
            status="done" if done_epic else "intake",
            project="web",
        )
    )
    for i in range(CHILDREN):
        child = _entry(f"{SCOPE}-c{i}", type="feature", project="web", parent=SCOPE)
        if i < done_children:
            child["status"] = "done"
        entries.append(child)
    path.write_text(json.dumps({"entries": entries}), encoding="utf-8")


def _invoke_drain() -> tuple[int, dict, float]:
    from fno.king.cli import agents_king_app

    started = time.monotonic()
    result = CliRunner().invoke(agents_king_app, ["drain", SCOPE])
    elapsed = time.monotonic() - started
    payload = json.loads(result.output.strip().splitlines()[-1])
    return result.exit_code, payload, elapsed


def test_repeat_fire_answers_inside_the_stopgate_budget_without_the_store(
    graph, monkeypatch
):
    budget = _stopgate_read_timeout()

    exit_code, payload, elapsed = _invoke_drain()
    assert exit_code == 0
    # Positive control: genuinely undelivered nodes survive the speedup.
    assert payload["undelivered"] == UNDELIVERED_COUNT
    assert "cached" not in payload
    assert elapsed < budget, f"fresh drain took {elapsed:.1f}s, budget {budget}s"

    # The second fire is the one the gate blocks on. The mechanical marker
    # is not the clock: the store read is made to raise, so a cached answer
    # that still touched the store cannot pass. Patched at fno.graph.store,
    # where read_entries' call-time import resolves.
    import fno.graph.store as store

    def _forbidden(*a, **k):
        raise AssertionError("cached drain re-read the store")

    monkeypatch.setattr(store, "read_graph_strict", _forbidden)
    monkeypatch.setattr(store, "read_graph", _forbidden)
    exit_code, payload, elapsed = _invoke_drain()
    assert exit_code == 0
    assert payload["undelivered"] == UNDELIVERED_COUNT
    assert payload.get("cached") is True
    assert elapsed < budget, f"cached drain took {elapsed:.1f}s, budget {budget}s"


def test_graph_change_invalidates_the_row(graph):
    _invoke_drain()
    _write_graph(graph, done_children=CHILDREN)  # children delivered, epic open

    exit_code, payload, _ = _invoke_drain()
    assert exit_code == 0
    assert payload["undelivered"] == 1
    assert "cached" not in payload

    _write_graph(graph, done_children=CHILDREN, done_epic=True)  # scope drained
    exit_code, payload, _ = _invoke_drain()
    assert exit_code == 0
    assert payload["undelivered"] == 0
    assert "cached" not in payload

    # And the delivered zero is itself cached: a cached zero here is a real
    # one, computed above from a graph that carried non-terminal rows.
    exit_code, payload, _ = _invoke_drain()
    assert exit_code == 0
    assert payload["undelivered"] == 0
    assert payload.get("cached") is True


def test_corrupt_cache_reads_as_a_miss(graph):
    from fno import paths

    cache = paths.state_dir() / "cache" / "king-drain.json"
    cache.parent.mkdir(parents=True, exist_ok=True)
    # Both shapes: unparseable bytes, and valid JSON with the wrong shape
    # (array root; a row whose ident matches but carries no count).
    ident = list(_ident(graph))
    cache.write_text("{not json", encoding="utf-8")
    exit_code, payload, _ = _invoke_drain()
    assert exit_code == 0
    assert payload["undelivered"] == UNDELIVERED_COUNT
    assert "cached" not in payload

    cache.write_text(json.dumps([1, 2]), encoding="utf-8")
    exit_code, payload, _ = _invoke_drain()
    assert exit_code == 0
    assert payload["undelivered"] == UNDELIVERED_COUNT
    assert "cached" not in payload

    cache.write_text(json.dumps({SCOPE: {"ident": ident}}), encoding="utf-8")
    exit_code, payload, _ = _invoke_drain()
    assert exit_code == 0
    assert payload["undelivered"] == UNDELIVERED_COUNT
    assert "cached" not in payload


def test_external_backend_never_serves_the_cache(graph, monkeypatch):
    from fno.king import drain_cache
    from fno.king.cli import agents_king_app

    ident = drain_cache.graph_ident(graph)
    assert ident is not None
    drain_cache.store(SCOPE, ident, undelivered=999)

    import fno.tracker as tracker

    monkeypatch.setattr(tracker, "active_backend_name", lambda: "external")
    result = CliRunner().invoke(agents_king_app, ["drain", SCOPE])
    assert result.exit_code != 0  # unreadable under an external backend, as before
    assert "999" not in result.output  # the poisoned cache row never surfaced


def test_wake_entries_read_once_per_graph_identity(graph, monkeypatch):
    import fno.pr_watch._king_wake as wake

    wake._WAKE_ENTRIES_MEMO.update(ident=None, entries=None)
    calls: list[int] = []

    def _counting_read(path):
        calls.append(1)
        from fno.graph.store import read_graph_strict

        return read_graph_strict(path)

    monkeypatch.setattr("fno.graph.store.read_graph", _counting_read)
    first = wake._graph_entries_for_wake()
    second = wake._graph_entries_for_wake()
    assert len(first) == FILLER + CHILDREN + 1
    assert first == second
    assert len(calls) == 1  # the unchanged graph is served, not re-read

    _write_graph(graph, done_children=CHILDREN, done_epic=True)
    third = wake._graph_entries_for_wake()
    assert len(calls) == 2  # the write moved the identity: one real re-read
    epic = next(row for row in third if row.get("id") == SCOPE)
    assert epic["status"] == "done"  # the fresh row, not the memo


def test_sqlite_backend_names_no_file_identity(graph):
    import sqlite3

    from fno.king import drain_cache

    assert drain_cache.graph_ident(graph) is not None
    con = sqlite3.connect(graph.with_suffix(".db"))
    try:
        con.execute(
            "CREATE TABLE IF NOT EXISTS graph_meta ("
            "key TEXT PRIMARY KEY, value TEXT NOT NULL)"
        )
        con.execute(
            "INSERT INTO graph_meta(key, value) VALUES('backend', 'sqlite')"
        )
        con.commit()
    finally:
        con.close()
    # The keeper serves graph.db under this backend; the json file can lag
    # until export, so no file identity may exist to key a cache on.
    assert drain_cache.graph_ident(graph) is None


def test_store_skipped_when_the_identity_moved_during_the_read(graph, monkeypatch):
    from fno.king import drain_cache

    real_ident = drain_cache.graph_ident(graph)
    assert real_ident is not None
    stats = {"n": 0}

    def _write_lands_mid_read(path):
        stats["n"] += 1
        if stats["n"] == 1:
            return real_ident  # the pre-read stat
        return ("moved",)  # a write landed before the post-read stat

    monkeypatch.setattr(drain_cache, "graph_ident", _write_lands_mid_read)
    exit_code, payload, _ = _invoke_drain()
    assert exit_code == 0
    assert payload["undelivered"] == UNDELIVERED_COUNT
    # The mismatched pair was never stored: the moved identity is not real,
    # and the true identity has no row, so the next fire recomputes.
    monkeypatch.setattr(drain_cache, "graph_ident", lambda p: real_ident)
    assert drain_cache.load(SCOPE, real_ident) is None
