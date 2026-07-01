"""CLI + fail-safe tests for the batch primitive (sigma-review gaps 1-4).

The exit codes are a hand-mapped API the Wave 2 selection path shells against
(BatchExists=3, NoOpenBatch=2, BatchFull=4, generic=1), so they get their own
tests; likewise the config coercers and the `_safe` path-traversal guard, which
sits on a trust boundary (domain flows straight into a filesystem path).
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.backlog import batch as B
from fno.config import BatchBlock, ConfigBlock

runner = CliRunner()


def _inv(*args):
    return runner.invoke(B.cli, list(args))


# --- exit-code contract ----------------------------------------------------


def test_open_then_join_exit_zero(tmp_path: Path) -> None:
    r = _inv("open", "-d", "code", "-b", "f", "-w", "w", "--root", str(tmp_path))
    assert r.exit_code == 0
    r = _inv("join", "-d", "code", "-n", "x-1", "--root", str(tmp_path))
    assert r.exit_code == 0


def test_open_twice_exit_3(tmp_path: Path) -> None:
    _inv("open", "-d", "code", "-b", "f", "-w", "w", "--root", str(tmp_path))
    r = _inv("open", "-d", "code", "-b", "f", "-w", "w", "--root", str(tmp_path))
    assert r.exit_code == 3  # BatchExists


def test_join_no_open_batch_exit_2(tmp_path: Path) -> None:
    r = _inv("join", "-d", "code", "-n", "x-1", "--root", str(tmp_path))
    assert r.exit_code == 2  # NoOpenBatch


def test_join_full_exit_4(tmp_path: Path) -> None:
    _inv("open", "-d", "code", "-b", "f", "-w", "w", "--max-nodes", "1", "--root", str(tmp_path))
    _inv("join", "-d", "code", "-n", "x-1", "--root", str(tmp_path))
    r = _inv("join", "-d", "code", "-n", "x-2", "--root", str(tmp_path))
    assert r.exit_code == 4  # BatchFull


def test_close_no_open_batch_exit_2(tmp_path: Path) -> None:
    r = _inv("close", "-d", "code", "--root", str(tmp_path))
    assert r.exit_code == 2


def test_abandon_no_open_batch_exit_2(tmp_path: Path) -> None:
    r = _inv("abandon", "-d", "code", "--root", str(tmp_path))
    assert r.exit_code == 2


def test_bad_domain_exit_1(tmp_path: Path) -> None:
    # `_safe` rejects a traversal domain -> BatchValidationError -> generic exit 1
    r = _inv("open", "-d", "../escape", "-b", "f", "-w", "w", "--root", str(tmp_path))
    assert r.exit_code == 1


# --- cli_policy fail-safe (the dangerous direction) ------------------------


def test_policy_node_lookup_failure_ships_solo(tmp_path: Path, monkeypatch) -> None:
    """A failed `fno backlog get` must degrade to ship_solo, never join/start."""
    import subprocess

    class _Fail:
        returncode = 1
        stdout = ""
        stderr = "boom"

    monkeypatch.setattr(subprocess, "run", lambda *a, **k: _Fail())
    # even with batching enabled, a lookup failure ships solo (conservative)
    monkeypatch.setattr(B, "_load_batch_enabled", lambda: True)
    r = _inv("policy", "-n", "x-unknown", "--root", str(tmp_path))
    assert r.exit_code == 0
    assert '"ship_solo"' in r.output


# --- _safe path-traversal trust boundary -----------------------------------


@pytest.mark.parametrize("bad", ["../x", "a/b", "", ".", "..", "  "])
def test_safe_rejects_traversal(tmp_path: Path, bad: str) -> None:
    with pytest.raises(B.BatchValidationError):
        B.open_batch(domain=bad, branch="f", worktree="w", root=tmp_path)


# --- config coercers -------------------------------------------------------


def test_enabled_coerces_non_bool_to_false() -> None:
    assert BatchBlock(enabled="maybe").enabled is False
    assert BatchBlock(enabled="true").enabled is True


def test_max_nodes_coerces_non_positive_to_3() -> None:
    assert BatchBlock(max_nodes=0).max_nodes == 3
    assert BatchBlock(max_nodes=-1).max_nodes == 3
    assert BatchBlock(max_nodes=5).max_nodes == 5


def test_max_loc_coerces_bad_to_none() -> None:
    assert BatchBlock(max_loc=-1).max_loc is None
    assert BatchBlock(max_loc=0).max_loc is None
    assert BatchBlock(max_loc="lots").max_loc is None
    assert BatchBlock(max_loc=500).max_loc == 500
    assert BatchBlock(max_loc=None).max_loc is None


def test_config_block_scalar_batch_degrades_not_raises() -> None:
    """A scalar `config.batch: 42` must NOT crash the whole settings load."""
    block = ConfigBlock(batch=42)
    assert block.batch.enabled is False  # degraded to default disabled block
