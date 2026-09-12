"""Tests for cli/src/fno/setup/shim_check.py (x-c911)."""

import os
import stat
from pathlib import Path

import pytest

from fno.setup import shim_check
from fno.setup.shim_check import main, repair, scan


def _make_durable(tmp_path: Path) -> Path:
    durable = tmp_path / "durable-bin"
    durable.mkdir()
    binary = durable / "fno-py"
    binary.write_text("#!/bin/sh\ntrue\n")
    binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
    return durable


def _dangling(bin_dir: Path, name: str = "fno-py") -> Path:
    link = bin_dir / name
    os.symlink(bin_dir.parent / "nowhere" / name, link)
    return link


@pytest.fixture()
def bin_dir(tmp_path: Path) -> Path:
    d = tmp_path / "bin"
    d.mkdir()
    return d


def test_dangling_fno_link_detected_and_unrelated_link_ignored(bin_dir):
    _dangling(bin_dir, "fno-py")
    os.symlink(bin_dir.parent / "nowhere" / "pyfiglet", bin_dir / "pyfiglet")

    report = scan(bin_dir)

    assert report["healthy"] is False
    defect = report["defects"][0]
    assert defect["name"] == "fno-py"
    assert defect["problem"] == "dangling"
    assert defect["repair"] is None, "no durable copy was ever created"
    assert not any(d["name"] == "pyfiglet" for d in report["defects"])


def test_temp_resolving_live_link_detected(bin_dir, tmp_path):
    staging = tmp_path / "staging"
    staging.mkdir()
    (staging / "fno-gh-proxy").write_text("#!/bin/sh\ntrue\n")
    os.symlink(staging / "fno-gh-proxy", bin_dir / "fno-gh-proxy")

    report = scan(bin_dir)

    assert report["healthy"] is False
    defect = report["defects"][0]
    assert defect["name"] == "fno-gh-proxy"
    assert defect["problem"] == "temp-resolving"


def test_sibling_dir_extending_the_temp_root_name_is_not_temp(bin_dir, tmp_path, monkeypatch):
    # A prefix test without a separator boundary reads a sibling whose name
    # extends the temp root's basename as inside temp.
    monkeypatch.setattr(shim_check, "_temp_root", lambda: str(tmp_path / "T"))
    sibling = tmp_path / "Ttools" / "bin"
    sibling.mkdir(parents=True)
    (sibling / "fno-py").write_text("#!/bin/sh\ntrue\n")
    os.symlink(sibling / "fno-py", bin_dir / "fno-py")

    report = scan(bin_dir)

    assert report["healthy"] is True, report


def test_healthy_link_outside_temp_is_clean(bin_dir):
    os.symlink("/usr/bin/true", bin_dir / "fno-footprint-cause")

    report = scan(bin_dir)

    assert report["healthy"] is True
    assert report["defects"] == []


def test_repair_repoints_to_the_durable_copy(bin_dir, tmp_path):
    _dangling(bin_dir, "fno-py")
    durable = _make_durable(tmp_path)
    report = scan(bin_dir)
    report["defects"][0]["repair"] = str(durable / "fno-py")

    remaining = repair(report["defects"])

    assert remaining == []
    assert os.path.realpath(bin_dir / "fno-py") == str(durable / "fno-py")


def test_repair_reports_unrepairable_without_a_durable_copy(bin_dir):
    _dangling(bin_dir, "fno-py")
    report = scan(bin_dir)
    assert report["defects"][0]["repair"] is None

    remaining = repair(report["defects"])

    assert len(remaining) == 1
    assert "fno-py" in remaining[0]
    assert "no durable copy" in remaining[0]


def test_main_exit_codes(bin_dir, tmp_path, monkeypatch):
    # The real durable bin lives outside the temp root; in tests it lands
    # under pytest's tmp_path, so point the temp root away or the repaired
    # link reads as temp-resolving.
    monkeypatch.setattr(shim_check, "UV_TOOL_FNO_BIN", tmp_path / "durable-bin")
    monkeypatch.setattr(shim_check, "_temp_root", lambda: "/fno-no-temp-root")

    assert main(["--bin-dir", str(bin_dir)]) == 0

    _dangling(bin_dir)
    assert main(["--bin-dir", str(bin_dir)]) == 1

    _make_durable(tmp_path)
    assert main(["--bin-dir", str(bin_dir), "--repair"]) == 0
    assert os.path.realpath(bin_dir / "fno-py") == str(tmp_path / "durable-bin" / "fno-py")
