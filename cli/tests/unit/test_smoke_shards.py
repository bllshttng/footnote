"""CI runs the smoke suite as two shards. They must cover the whole registry.

The workflow splits `smoke` into `smoke-pytest` (`--only 'Sync + build,Pytest*'`)
and `smoke-rest` (`--skip 'Pytest*'`) so the two run in parallel on separate
runners. That halves wall clock only if nothing is lost in the split, so this
test asserts the cover against the registry the runner actually builds.

The selections below are the literal strings the workflow passes. Change one
there and this test fails, which is the point: the workflow and the cover
cannot drift apart silently.
"""
from __future__ import annotations

from pathlib import Path

from fno.test_cmd import _name_matches, smoke_steps

# Verbatim from .github/workflows/cli-ci.yml.
PYTEST_SHARD_ONLY = "Sync + build,Pytest*"
REST_SHARD_SKIP = "Pytest*"

_REPO_ROOT = Path(__file__).resolve().parents[3]


def _names() -> list[str]:
    return [name for name, _cwd, _cmd in smoke_steps(_REPO_ROOT)]


def _pytest_shard(names: list[str]) -> list[str]:
    return [n for n in names if _name_matches(n, PYTEST_SHARD_ONLY)]


def _rest_shard(names: list[str]) -> list[str]:
    return [n for n in names if not _name_matches(n, REST_SHARD_SKIP)]


def test_the_two_shards_cover_every_step() -> None:
    """Every registry step runs in at least one shard.

    Stated as a positive marker per step, never as "nothing was uncovered":
    an empty registry and a fully covered one both have zero uncovered steps,
    and only one of them is a real cover.
    """
    names = _names()
    assert names, "the smoke registry is empty; this test would pass vacuously"
    covered = set(_pytest_shard(names)) | set(_rest_shard(names))
    for name in names:
        assert name in covered, f"step {name!r} runs in neither CI shard"


def test_pytest_runs_in_exactly_one_shard() -> None:
    """The expensive half must not run twice; it is 49 percent of the suite."""
    names = _names()
    assert "Pytest (unit + integration)" in _pytest_shard(names)
    assert "Pytest (unit + integration)" not in _rest_shard(names)


def test_the_rust_binary_is_built_in_the_shard_that_needs_it() -> None:
    """The seam is the faithful-ordering guard, so the guard must stay true.

    When pytest is selected the runner DELETES the fno-agents debug binary so
    the @requires_rust parity tests skip. The rust journey steps that need the
    binary therefore have to live in the shard that also carries its build
    step, and pytest has to live in the shard that does not.
    """
    names = _names()
    build = "Build fno-agents debug binary (for journey tests)"
    assert build in names, "the build step was renamed; re-check the shard seam"
    assert build in _rest_shard(names)
    assert build not in _pytest_shard(names)


def test_sync_and_build_is_the_only_step_both_shards_run() -> None:
    """A prerequisite is allowed to run twice; a test is not.

    Sync + build costs about three seconds and removes any question of whether
    pytest can rely on `uv build` having run. Anything else in both shards is
    duplicated work that should be moved to one side.
    """
    names = _names()
    both = sorted(set(_pytest_shard(names)) & set(_rest_shard(names)))
    assert both == ["Sync + build"]
