"""Keep the smoke-shard per-test ceiling measured against its step cap.

The 720s step cap is split across thirteen shards, and a passing test close to
the 120s timeout leaves too little margin to absorb two hangs. These guards
keep the timeout and markers low enough while naming any future test that
builds a fork-default pool in a threaded xdist worker.
"""
from __future__ import annotations

import re
import tomllib
from pathlib import Path

_PYPROJECT = Path(__file__).resolve().parents[2] / "pyproject.toml"

# The smoke-pytest step's twelve-minute cap. Keep each per-test timeout to
# one fifth of that cap so a shard can absorb two hangs.
_STEP_CAP_SECONDS = 12 * 60
_PER_TEST_BOUND_SECONDS = _STEP_CAP_SECONDS // 5


def _config() -> dict:
    return tomllib.loads(_PYPROJECT.read_text())


def test_pytest_timeout_is_a_dev_dependency() -> None:
    dev = _config()["dependency-groups"]["dev"]
    assert any(str(dep).startswith("pytest-timeout") for dep in dev), dev


def test_ini_timeout_lets_a_shard_absorb_two_hangs() -> None:
    timeout = _config()["tool"]["pytest"]["ini_options"]["timeout"]
    assert isinstance(timeout, (int, float)), timeout
    assert 0 < timeout <= _PER_TEST_BOUND_SECONDS, timeout


def test_no_test_builds_a_fork_default_pool() -> None:
    tests = Path(__file__).resolve().parents[1]
    pool = re.compile(r"\b(?:multiprocessing|mp)\.Pool\(")
    hits = [
        f"{path.relative_to(tests)}:{line_number}"
        for path in tests.rglob("*.py")
        for line_number, line in enumerate(path.read_text().splitlines(), 1)
        if pool.search(line)
    ]
    assert not hits, (
        "use multiprocessing.get_context('spawn').Pool(...): fork inside a "
        "threaded xdist worker can deadlock the child on Linux; found "
        + ", ".join(hits)
    )


def test_every_timeout_marker_is_under_the_bound() -> None:
    tests = Path(__file__).resolve().parents[1]
    marker = re.compile(r"mark\.timeout\(([^)]*)\)")
    for path in tests.rglob("*.py"):
        for line_number, line in enumerate(path.read_text().splitlines(), 1):
            match = marker.search(line)
            if match:
                timeout = float(match.group(1).split(",", 1)[0].strip())
                assert timeout <= _PER_TEST_BOUND_SECONDS, (
                    f"{path.relative_to(tests)}:{line_number} timeout "
                    f"{timeout}s exceeds {_PER_TEST_BOUND_SECONDS}s"
                )
