"""Unit tests for the per-repo branch-provenance cache.

The cache is the join between the pr-watch tick's stranded sweep (writer)
and the Kanban board (reader), so the contract here is the render
contract: only non-CLEAN rows are stored, and every read failure degrades
to "no section".
"""
import json
from pathlib import Path

import pytest

from fno.branch_provenance_cache import CACHE_RELPATH, provenance_lines, read_cache, write_cache
from fno.worktree_stranded import CLEAN, STRANDED, Row


def cache_path(repo):
    return Path(repo) / CACHE_RELPATH


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
        "klass": "STRANDED",
        "unpushed": 23,
        "has_remote": False,
        "age": "33 hours ago",
        "pr_number": None,
        "live": False,
        "path": "/wt/x-abcd",
    }


def test_unmapped_node_is_cached_not_dropped(tmp_path):
    unmapped = _row(
        STRANDED, None, 7, "2 hours ago", has_remote=False, pr_number=None, live=False
    )
    write_cache(tmp_path, [unmapped])
    assert read_cache(tmp_path) == [
        {
            "branch": "feature/unmapped",
            "node": None,
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
    cache_path(tmp_path).write_text("[1, 2]")
    assert read_cache(tmp_path) == []  # a list of non-objects is not rows


def test_read_cache_drops_non_object_rows(tmp_path):
    cache_path(tmp_path).parent.mkdir()
    cache_path(tmp_path).write_text('[{"node": "x-1"}, 7, "junk"]')
    assert read_cache(tmp_path) == [{"node": "x-1"}]


def test_write_overwrites_never_appends(tmp_path):
    write_cache(tmp_path, [_stranded()])
    write_cache(tmp_path, [])
    assert read_cache(tmp_path) == []
    assert list(tmp_path.glob("*.tmp")) == []


def test_write_failure_never_raises(tmp_path):
    # A repo path that cannot hold the file (a regular file where the
    # directory belongs) must log, answer False, and leave the tick leg up.
    blocker = tmp_path / "not-a-dir"
    blocker.write_text("occupied")
    assert write_cache(blocker, [_stranded()]) is False
    assert read_cache(blocker) == []


@pytest.mark.parametrize("klass", ["UNKNOWN", "LIVE", "PR_OPEN", "ABANDONED", "SHIPPED"])
def test_every_reported_class_is_cached(tmp_path, klass):
    row = _row(klass, "x-1", 2, "3 hours ago", has_remote=True, pr_number=9, live=klass == "LIVE")
    write_cache(tmp_path, [row])
    assert [r["klass"] for r in read_cache(tmp_path)] == [klass]


# -- the board section the cache feeds --


def _cache_rows():
    return [
        {
            "branch": "feature/x-04ce-review-fixes",
            "node": "x-04ce",
            "klass": "STRANDED",
            "unpushed": 23,
            "has_remote": False,
            "age": "33 hours ago",
            "pr_number": None,
            "live": False,
            "path": "/wt/a",
        },
        {
            "branch": "fix/x-129b-payload-cache-head",
            "node": None,
            "klass": "UNKNOWN",
            "unpushed": 0,
            "has_remote": True,
            "age": "12 hours ago",
            "pr_number": None,
            "live": True,
            "path": "/wt/b",
        },
    ]


def test_provenance_lines_render_mapped_and_unmapped_rows(tmp_path):
    """An unmapped branch is exactly the interesting case: rendered by name,
    never dropped."""
    cache_path(tmp_path).parent.mkdir(parents=True)
    cache_path(tmp_path).write_text(json.dumps(_cache_rows()))

    lines = provenance_lines([tmp_path])

    assert lines[0] == "## Branch Provenance"
    assert (
        "- **x-04ce** (feature/x-04ce-review-fixes): "
        "no remote, 23 unpushed, no PR, newest commit 33 hours ago" in lines
    )
    assert (
        "- *(unmapped)* (fix/x-129b-payload-cache-head): "
        "has remote, 0 unpushed, no PR, LIVE, newest commit 12 hours ago" in lines
    )


def test_provenance_lines_omitted_when_cache_empty(tmp_path):
    assert provenance_lines([tmp_path]) == []


def test_provenance_lines_fail_open_on_bad_cache(tmp_path):
    cache_path(tmp_path).parent.mkdir(parents=True)
    cache_path(tmp_path).write_text("{not json")
    assert provenance_lines([tmp_path]) == []
