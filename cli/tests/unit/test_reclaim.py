"""Tests for the reclaim janitor (x-7ca7 task 4.1)."""
from __future__ import annotations

import json
import os
import time
from pathlib import Path

import pytest

from fno import reclaim as reclaim_mod


@pytest.fixture()
def temp_root(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> Path:
    monkeypatch.setattr(reclaim_mod, "_temp_root", lambda: tmp_path)
    return tmp_path


def _aged(path: Path, minutes: int) -> None:
    old = time.time() - minutes * 60
    os.utime(path, (old, old))


def test_only_old_leaked_homes_are_selected(temp_root: Path) -> None:
    old_home = temp_root / ".tmpOLD"
    (old_home / ".cache" / "fno-bootstrap").mkdir(parents=True)
    _aged(old_home, 180)  # 3 hours
    fresh_home = temp_root / ".tmpNEW"
    (fresh_home / ".cache" / "fno-bootstrap").mkdir(parents=True)
    _aged(fresh_home, 1)  # 1 minute
    plain_old = temp_root / ".tmpPLAIN"
    plain_old.mkdir()
    _aged(plain_old, 180)  # old but holds no fno state

    found = reclaim_mod._leaked_test_homes()
    assert old_home in found
    assert fresh_home not in found
    assert plain_old not in found


def test_apply_removes_the_old_lane_and_writes_the_receipt(
    temp_root: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    old_home = temp_root / ".tmpOLD"
    (old_home / ".cache" / "fno-bootstrap").mkdir(parents=True)
    (old_home / ".cache" / "uv" / "blobs").mkdir(parents=True)
    (old_home / ".cache" / "uv" / "blobs" / "payload").write_bytes(b"x" * 4096)
    _aged(old_home, 180)
    fresh_home = temp_root / ".tmpNEW"
    (fresh_home / ".cache" / "fno-bootstrap").mkdir(parents=True)
    _aged(fresh_home, 1)

    state = tmp_path / "state"
    monkeypatch.setattr(reclaim_mod, "_receipt_path", lambda: state / "reclaim" / "last-run.json")
    monkeypatch.setattr(reclaim_mod, "_uv_cache_dir", lambda: None)  # no uv in the test box

    lanes = reclaim_mod.run_reclaim(apply=True)
    assert not old_home.exists(), "the aged fake HOME is removed"
    assert fresh_home.exists(), "the fresh fake HOME survives"
    leak_lane = next(l for l in lanes if l.name == "leaked_test_homes")
    assert leak_lane.count == 1 and leak_lane.bytes > 0

    receipt = json.loads((state / "reclaim" / "last-run.json").read_text())
    assert receipt["lanes"]["leaked_test_homes"]["paths"] == 1
    assert receipt["total_bytes"] > 0


def test_stale_scratch_lane_uses_one_day(temp_root: Path) -> None:
    scratch = temp_root / "fno-parity-junk"
    scratch.mkdir()
    _aged(scratch, 25 * 60)
    fresh = temp_root / "fno-fresh"
    fresh.mkdir()
    _aged(fresh, 10)
    assert reclaim_mod._stale_test_scratch() == [scratch]
