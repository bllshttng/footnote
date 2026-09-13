"""cargo_build_dir removal helper: ownership conjuncts + best-effort degrade."""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

from fno.cargo_build_dir import build_dir_base, remove_build_dir_for_worktree


@pytest.fixture()
def cargo_base(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    base = tmp_path / "base"
    monkeypatch.setenv("FNO_CARGO_TARGETS_BASE", str(base))
    return base


def _stub_cargo(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, answer: str
) -> None:
    bindir = tmp_path / "stubbin"
    bindir.mkdir(exist_ok=True)
    script = bindir / "cargo"
    script.write_text("#!/bin/sh\n" + answer + "\n")
    script.chmod(0o755)
    monkeypatch.setenv("PATH", str(bindir) + os.pathsep + os.environ["PATH"])


def _canned_metadata(directory: Path) -> str:
    return "printf '%s' '" + json.dumps({"build_directory": str(directory)}) + "'"


def _workspace(tmp_path: Path) -> Path:
    wt = tmp_path / "wt"
    (wt / "crates" / "x").mkdir(parents=True)
    (wt / "crates" / "x" / "Cargo.toml").touch()
    return wt


def _planted_hash_dir(base: Path) -> Path:
    d = base / "ab" / "c001"
    d.mkdir(parents=True)
    (d / "CACHEDIR.TAG").write_text("Signature: 87496387e5a84b3bb5c64e56a51a4e63\n")
    return d


def test_removes_owned_hash_dir(
    tmp_path: Path, cargo_base: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    d = _planted_hash_dir(cargo_base)
    _stub_cargo(monkeypatch, tmp_path, _canned_metadata(d))
    wt = _workspace(tmp_path)
    assert remove_build_dir_for_worktree(wt) is True
    assert not d.exists()


def test_keeps_out_of_base_resolution(
    tmp_path: Path, cargo_base: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    outside = tmp_path / "outside" / "c002"
    outside.mkdir(parents=True)
    (outside / "CACHEDIR.TAG").write_text("Signature: 87496387e5a84b3bb5c64e56a51a4e63\n")
    _stub_cargo(monkeypatch, tmp_path, _canned_metadata(outside))
    wt = _workspace(tmp_path)
    assert remove_build_dir_for_worktree(wt) is False
    assert outside.exists()


def test_degrades_when_cargo_fails(
    tmp_path: Path, cargo_base: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    d = _planted_hash_dir(cargo_base)
    _stub_cargo(monkeypatch, tmp_path, "exit 1")
    wt = _workspace(tmp_path)
    assert remove_build_dir_for_worktree(wt) is False
    assert d.exists()


def test_missing_tag_kept(
    tmp_path: Path, cargo_base: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    d = _planted_hash_dir(cargo_base)
    (d / "CACHEDIR.TAG").unlink()
    _stub_cargo(monkeypatch, tmp_path, _canned_metadata(d))
    wt = _workspace(tmp_path)
    assert remove_build_dir_for_worktree(wt) is False
    assert d.exists()


def test_build_dir_base_honors_env_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("FNO_CARGO_TARGETS_BASE", str(tmp_path / "custom"))
    assert build_dir_base() == tmp_path / "custom"
