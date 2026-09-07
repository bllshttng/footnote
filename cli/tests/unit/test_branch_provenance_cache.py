"""Unit tests for the per-repo branch-provenance cache.

The cache is the join between the pr-watch tick's stranded sweep (writer)
and the Kanban board (reader), so the contract here is the render
contract: only non-CLEAN rows are stored, titles come from the graph at
write time, and every read failure degrades to "no section".
"""
import json

import pytest

from fno.branch_provenance_cache import CACHE_RELPATH, cache_path, read_cache, write_cache
from fno.worktree_stranded import CLEAN, STRANDED, Row


def _row(klass, node, unpushed, age, **facts):
    name = node or "unmapped"
    base = {"path": f"/wt/{name}", "branch": f"feature/{name}"}
    base.update(facts)
    return Row(klass, node, unpushed, age, base)


def _stranded():
    return _row(
        STRANDED,
        "x-abcd",
        23,
        "33 hours ago",
        has_remote=False,
        pr_number=None,
        live=False,
    )


def test_write_skips_clean_rows_and_carries_the_full_shape(tmp_path):
    clean = _row(CLEAN, "x-ok", 0, "1 hour ago", has_remote=True, live=True)
    assert write_cache(tmp_path, [_stranded(), clean]) is True

    rows = json.loads((tmp_path / CACHE_RELPATH).read_text())
    assert len(rows) == 1  # a caught-up worktree has nothing to report
    assert rows[0] == {
        "branch": "feature/x-abcd",
        "node": "x-abcd",
        "node_title": None,
        "klass": "STRANDED",
        "unpushed": 23,
        "has_remote": False,
        "age": "33 hours ago",
        "pr_number": None,
        "live": False,
        "path": "/wt/x-abcd",
    }


def test_write_resolves_node_titles_from_entries_by_id(tmp_path):
    entries = {"x-abcd": {"title": "Branch provenance on the board"}}
    write_cache(tmp_path, [_stranded()], entries_by_id=entries)
    assert read_cache(tmp_path)[0]["node_title"] == "Branch provenance on the board"


def test_unmapped_node_is_cached_not_dropped(tmp_path):
    unmapped = _row(
        STRANDED, None, 7, "2 hours ago", has_remote=False, pr_number=None, live=False
    )
    write_cache(tmp_path, [unmapped])
    assert read_cache(tmp_path) == [
        {
            "branch": "feature/unmapped",
            "node": None,
            "node_title": None,
            "klass": "STRANDED",
            "unpushed": 7,
            "has_remote": False,
            "age": "2 hours ago",
            "pr_number": None,
            "live": False,
            "path": "/wt/unmapped",
        }
    ]


def test_read_cache_fails_open(tmp_path):
    assert read_cache(tmp_path) == []  # missing file
    cache_path(tmp_path).parent.mkdir()
    cache_path(tmp_path).write_text("{not json")
    assert read_cache(tmp_path) == []  # malformed
    cache_path(tmp_path).write_text('{"a": 1}')
    assert read_cache(tmp_path) == []  # wrong shape entirely


def test_write_overwrites_never_appends(tmp_path):
    write_cache(tmp_path, [_stranded()])
    write_cache(tmp_path, [])
    assert read_cache(tmp_path) == []
    leftovers = [p for p in tmp_path.glob("*.tmp")]
    assert leftovers == []


def test_write_failure_never_raises(tmp_path, monkeypatch):
    # A repo path that cannot hold the file (a regular file where the
    # directory belongs) must log, answer False, and leave the tick leg up.
    blocker = tmp_path / "not-a-dir"
    blocker.write_text("occupied")
    assert write_cache(blocker, [_stranded()]) is False
    assert read_cache(blocker) == []


def test_bad_graph_read_still_writes_rows(tmp_path, monkeypatch):
    import fno.graph.store as store

    def _boom():
        raise RuntimeError("graph unavailable")

    monkeypatch.setattr(store, "read_graph_strict", _boom)
    assert write_cache(tmp_path, [_stranded()]) is True
    assert read_cache(tmp_path)[0]["node"] == "x-abcd"


def test_cache_path_shape(tmp_path):
    assert cache_path(tmp_path) == tmp_path / ".fno" / "branch-provenance.json"


@pytest.mark.parametrize("klass", ["UNKNOWN", "LIVE", "PR_OPEN", "ABANDONED", "SHIPPED"])
def test_every_reported_class_is_cached(tmp_path, klass):
    row = _row(klass, "x-1", 2, "3 hours ago", has_remote=True, pr_number=9, live=klass == "LIVE")
    write_cache(tmp_path, [row])
    assert [r["klass"] for r in read_cache(tmp_path)] == [klass]
