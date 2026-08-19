"""Unit tests for the overlap hold (x-11ab).

Two seams, both repository-free: the pure intersection `_overlaps`, and the
fail-closed probe contract on `_base_move_paths` / `_pr_changed_paths` (a miss
is None with a stderr breadcrumb, a truncated compare is a miss). The
run_merge-level outcomes (held vs merged through the guard) live in
test_pr_merge.py beside the FakeRun fixtures they need.
"""
from __future__ import annotations

import json

import pytest

from fno.pr import _merge
from fno.pr._proc import Result


# ---- the pure decision ----


def test_overlaps_names_the_shared_paths():
    assert _merge._overlaps(
        ["cli/src/a.py", "cli/src/b.py"], ["cli/src/b.py", "cli/src/c.py"]
    ) == ["cli/src/b.py"]


def test_overlaps_empty_when_disjoint():
    # The AC1 shape: a base move touching nothing the PR touches cannot carry
    # a semantic conflict, so the guard must read this as "merge".
    assert _merge._overlaps(["crates/other.rs"], ["cli/src/this_pr.py"]) == []


def test_overlaps_drops_documentation_from_both_sides():
    # The AC3 shape: a docs-only overlap is not an overlap. Doc paths count as
    # documentation on EITHER side, whatever the other side carries.
    assert (
        _merge._overlaps(
            ["docs/guide.md", "cli/src/a.py"], ["docs/guide.md", "notes.md"]
        )
        == []
    )


def test_overlaps_dedups_and_sorts():
    assert _merge._overlaps(["b.py", "a.py", "a.py"], ["a.py", "b.py", "b.py"]) == [
        "a.py",
        "b.py",
    ]


# ---- the probes: fail CLOSED, miss = None + breadcrumb ----


def _fake_gh(monkeypatch, pr_refs: dict, compare: Result, files: Result | None = None):
    def run(args, cwd):
        if "baseRefName,headRefName" in args:
            return Result(0, json.dumps(pr_refs) + "\n", "")
        if any("/compare/" in a for a in args):
            return compare
        if "[.files[].path]" in args:
            return files if files is not None else Result(1, "", "gh failed")
        return Result(1, "", "unexpected call")

    monkeypatch.setattr(_merge, "_gh", run)


_REFS = {"baseRefName": "main", "headRefName": "feature/x"}


def test_base_move_paths_parses_the_reverse_compare(monkeypatch):
    payload = {"truncated": False, "names": ["cli/src/a.py", "docs/b.md"]}
    _fake_gh(monkeypatch, _REFS, Result(0, json.dumps(payload) + "\n", ""))
    assert _merge._base_move_paths(42, ".") == ["cli/src/a.py", "docs/b.md"]


@pytest.mark.parametrize(
    "payload",
    [
        {"truncated": True, "names": ["a.py"]},  # GitHub flagged the cap
        {"truncated": False, "names": [f"f{i}.py" for i in range(300)]},  # at the cap
        {"names": "not-a-list"},  # shape drift
        {"truncated": False},  # no file list at all
    ],
)
def test_base_move_paths_treats_these_as_misses(monkeypatch, capsys, payload):
    # Each of these under-reports or misreports the base move, and an unknown
    # move must hold, not merge: the miss contract is None.
    _fake_gh(monkeypatch, _REFS, Result(0, json.dumps(payload) + "\n", ""))
    assert _merge._base_move_paths(42, ".") is None
    assert "overlap probe unavailable" in capsys.readouterr().err


def test_base_move_paths_unparseable_output_is_a_miss(monkeypatch, capsys):
    _fake_gh(monkeypatch, _REFS, Result(0, "not json\n", ""))
    assert _merge._base_move_paths(42, ".") is None
    assert "overlap probe unavailable" in capsys.readouterr().err


def test_base_move_paths_gh_failure_is_a_miss(monkeypatch, capsys):
    _fake_gh(monkeypatch, _REFS, Result(1, "", "gh: down"))
    assert _merge._base_move_paths(42, ".") is None
    assert "overlap probe unavailable" in capsys.readouterr().err


def test_pr_changed_paths_parses_and_empty_is_not_a_miss(monkeypatch):
    # An empty diff cannot overlap anything: [] means "proved no overlap",
    # None stays reserved for "could not tell".
    _fake_gh(
        monkeypatch,
        _REFS,
        Result(0, "{}\n", ""),
        files=Result(0, json.dumps(["cli/src/a.py"]) + "\n", ""),
    )
    assert _merge._pr_changed_paths(42, ".") == ["cli/src/a.py"]


def test_pr_changed_paths_failure_is_a_miss(monkeypatch, capsys):
    _fake_gh(monkeypatch, _REFS, Result(0, "{}\n", ""), files=Result(1, "", "gh: down"))
    assert _merge._pr_changed_paths(42, ".") is None
    assert "overlap probe unavailable" in capsys.readouterr().err
