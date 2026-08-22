"""`--only` / `--skip` selector contract for `fno doctor test smoke`.

The two flags are exact complements over the same glob list, which is what
lets CI run the suite as two shards that cover the registry by construction
rather than by a hand-maintained list of step names.
"""
from __future__ import annotations

import pytest

from fno.test_cmd import _name_matches, _parse_smoke_args


def test_skip_parses_and_carries_its_value() -> None:
    opts = _parse_smoke_args(["--skip", "Pytest*"])
    assert opts["skip"] == "Pytest*"
    assert _parse_smoke_args(["--skip=Pytest*"])["skip"] == "Pytest*"


def test_skip_refuses_an_empty_or_forgotten_value() -> None:
    """An unset shell variable is a caller bug, never an instruction.

    `--skip=` accepted silently would run the FULL suite while reading as the
    shard the caller asked for, which is the same defect `--only=` already
    guards against.
    """
    for argv in (["--skip="], ["--skip"], ["--skip", "--retry-failed"]):
        with pytest.raises(ValueError, match="needs a value"):
            _parse_smoke_args(argv)


def test_skip_is_its_own_subset_mode() -> None:
    """Two subset modes have no defined precedence, so pairing them refuses."""
    for argv in (["--only", "a", "--skip", "b"], ["--skip", "b", "--changed"],
                 ["--skip", "b", "--retry-failed"]):
        with pytest.raises(ValueError, match="separate subset modes"):
            _parse_smoke_args(argv)


def test_name_matches_any_glob_in_a_comma_list() -> None:
    globs = "Sync + build,Pytest*"
    assert _name_matches("Sync + build", globs)
    assert _name_matches("Pytest (unit + integration)", globs)
    assert not _name_matches("ruff + mypy (both repo-wide)", globs)


def test_name_matches_tolerates_spacing_and_empty_entries() -> None:
    assert _name_matches("Pytest (unit + integration)", " Pytest* , ")
    assert not _name_matches("anything", ",")


def test_a_step_name_containing_a_comma_still_selects_exactly() -> None:
    """Three registry steps have a comma in their own name.

    Splitting the value before trying it whole turned `--only '<exact name>'`
    into two globs matching nothing, so the documented way to reproduce one CI
    step locally exited 1 for those three. The whole value is tried first.
    """
    commad = "Cross-impl claims compat matrix (merge gate; fails loudly, never skips here)"
    assert _name_matches(commad, commad)
    assert not _name_matches("Sync + build", commad)
    # The list form still works beside it.
    assert _name_matches("Sync + build", "Sync + build,Pytest*")


def test_only_and_skip_partition_any_name_list() -> None:
    """The property CI depends on: every name lands in exactly one selector."""
    names = ["Sync + build", "Pytest (unit + integration)", "ruff + mypy",
             "Build fno-agents debug binary (for journey tests)"]
    globs = "Pytest*"
    kept = [n for n in names if _name_matches(n, globs)]
    dropped = [n for n in names if not _name_matches(n, globs)]
    assert kept == ["Pytest (unit + integration)"]
    assert sorted(kept + dropped) == sorted(names)
