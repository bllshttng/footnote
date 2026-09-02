"""Tests for the pytest collection shard used by the smoke matrix."""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

from tests.conftest import pytest_collection_modifyitems
from tests.conftest import partition_pytest_nodeids

_CLI_ROOT = Path(__file__).resolve().parents[2]


class _Item:
    def __init__(self, nodeid: str) -> None:
        self.nodeid = nodeid

    def add_marker(self, marker: object) -> None:
        del marker


def test_AC2_HP_shard_selects_one_disjoint_slice(monkeypatch) -> None:
    nodeids = [f"tests/unit/test_real_{index}.py::test_case" for index in range(8)]
    items = [_Item(nodeid) for nodeid in nodeids]
    monkeypatch.setenv("FNO_PYTEST_SHARD", "2/4")

    pytest_collection_modifyitems(items)

    assert [item.nodeid for item in items] == [nodeids[1], nodeids[5]]


def _real_collected_nodeids(monkeypatch) -> list[str]:
    env = os.environ.copy()
    env.pop("FNO_PYTEST_SHARD", None)
    result = subprocess.run(
        [sys.executable, "-m", "pytest", "--collect-only", "-q", "tests/unit"],
        cwd=_CLI_ROOT,
        env=env,
        check=True,
        capture_output=True,
        text=True,
    )
    monkeypatch.delenv("FNO_PYTEST_SHARD", raising=False)
    return [
        line.strip()
        for line in result.stdout.splitlines()
        if line.startswith("tests/") and "::" in line
    ]


@pytest.mark.parametrize("total", [1, 2, 4, 7])
def test_AC2_HP_real_collected_nodeids_are_disjoint_and_complete(
    total: int, monkeypatch
) -> None:
    nodeids = _real_collected_nodeids(monkeypatch)
    assert nodeids, "the unit suite collected no nodeids"

    shards = [
        partition_pytest_nodeids(nodeids, f"{index}/{total}")
        for index in range(1, total + 1)
    ]

    assert set().union(*map(set, shards)) == set(nodeids)
    for left_index, left in enumerate(shards):
        for right in shards[left_index + 1:]:
            assert set(left).isdisjoint(right)


@pytest.mark.parametrize("spec", ["0/4", "5/4", "1/0", "four/4", "1/4/2"])
def test_AC9_EDGE_malformed_pytest_shard_fails_loudly(spec: str) -> None:
    with pytest.raises(pytest.UsageError, match="FNO_PYTEST_SHARD"):
        partition_pytest_nodeids(["tests/unit/test_real.py::test_case"], spec)


def test_AC6_ERR_empty_pytest_shard_fails_loudly() -> None:
    with pytest.raises(pytest.UsageError, match="selected no tests"):
        partition_pytest_nodeids(["tests/unit/test_real.py::test_case"], "2/2")
