"""The smoke-shard hang must name itself.

Measured 2026-09-17: six smoke-pytest shards died at the job cap with no
failing assertion and no test name anywhere in the log, while 1121 passing
shards topped out at 778s against a 900s job cap. pytest-timeout is the
ceiling that turns the next hang into a named failure carrying a stack; this
guard keeps a `uv sync` or a tidy-up from quietly dropping it.
"""
from __future__ import annotations

import tomllib
from pathlib import Path

_PYPROJECT = Path(__file__).resolve().parents[2] / "pyproject.toml"

# The smoke-pytest step's own timeout-minutes. A per-test ceiling above the
# clock that kills the step can never fire.
_STEP_CAP_SECONDS = 12 * 60


def _config() -> dict:
    return tomllib.loads(_PYPROJECT.read_text())


def test_pytest_timeout_is_a_dev_dependency() -> None:
    dev = _config()["dependency-groups"]["dev"]
    assert any(str(dep).startswith("pytest-timeout") for dep in dev), dev


def test_ini_timeout_is_positive_and_under_the_step_cap() -> None:
    timeout = _config()["tool"]["pytest"]["ini_options"]["timeout"]
    assert isinstance(timeout, (int, float)), timeout
    assert 0 < timeout <= _STEP_CAP_SECONDS, timeout
