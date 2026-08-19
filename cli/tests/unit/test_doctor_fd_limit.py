"""Unit tests for the fno doctor open-file-limit advisory.

The report exists because two observers disagreed and both were right: a
login shell reads 1048576 while every launchd-spawned worker runs at 256.
The verdict computation must be testable without a Mac, so the launchctl
line is parsed by a pure function and the platform probe is monkeypatched.
"""
from __future__ import annotations

import resource
import sys
from pathlib import Path
from typing import Any, Optional

import pytest

from fno import doctor
from fno.doctor import _fd_limit_report, _parse_launchctl_maxfiles


def test_parse_launchctl_maxfiles_reads_soft_limit() -> None:
    assert _parse_launchctl_maxfiles("maxfiles\t256\tunlimited\n") == 256
    assert _parse_launchctl_maxfiles("  maxfiles 65536 unlimited\n") == 65536


def test_parse_launchctl_maxfiles_rejects_junk() -> None:
    assert _parse_launchctl_maxfiles("") is None
    assert _parse_launchctl_maxfiles("maxfiles unlimited unlimited\n") is None
    assert _parse_launchctl_maxfiles("some\tother\tlimits\n") is None


def _report_for(
    monkeypatch: pytest.MonkeyPatch,
    soft: int,
    launchctl: Optional[tuple[int, str]],
) -> dict[str, Any]:
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setattr(
        resource, "getrlimit", lambda _which: (soft, resource.RLIM_INFINITY)
    )

    def fake_bounded(argv: list[str]) -> Optional[tuple[int, str, str]]:
        if argv[0] == "launchctl":
            return (launchctl[0], launchctl[1], "") if launchctl else (1, "", "")
        if argv[0] == "sysctl":
            return (0, "491520\n", "")
        return None

    monkeypatch.setattr(doctor, "_bounded_command", fake_bounded)
    return _fd_limit_report()


def test_low_launchd_default_verdict_low(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC3 shape: healthy shell beside a starving launchd session default."""
    report = _report_for(monkeypatch, 1048576, (0, "maxfiles\t256\tunlimited\n"))
    assert report["soft"] == 1048576
    assert report["launchd_soft"] == 256
    assert report["kern_maxfiles"] == 491520
    assert report["verdict"] == "low"


def test_low_soft_with_no_launchctl_probe_verdict_low(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The probe failing must not hide a low reading of THIS process."""
    report = _report_for(monkeypatch, 256, None)
    assert report["launchd_soft"] is None
    assert report["verdict"] == "low"


def test_healthy_limits_verdict_ok(monkeypatch: pytest.MonkeyPatch) -> None:
    report = _report_for(monkeypatch, 65536, (0, "maxfiles 65536 unlimited\n"))
    assert report["verdict"] == "ok"


def test_hard_limit_reported_as_unlimited_string(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    report = _report_for(monkeypatch, 1048576, (0, "maxfiles 256 unlimited\n"))
    assert report["hard"] == "unlimited"


def test_non_darwin_skips_launchd_probe(monkeypatch: pytest.MonkeyPatch) -> None:
    """On linux the launchd branch must not run; the report still resolves."""
    monkeypatch.setattr(sys, "platform", "linux")
    monkeypatch.setattr(
        resource, "getrlimit", lambda _which: (65536, resource.RLIM_INFINITY)
    )

    def no_launchctl(argv: list[str]) -> Optional[tuple[int, str, str]]:
        assert argv[0] != "launchctl", "launchctl must not run off darwin"
        return None

    monkeypatch.setattr(doctor, "_bounded_command", no_launchctl)
    report = _fd_limit_report()
    assert report["launchd_soft"] is None
    assert report["verdict"] == "ok"


def test_linux_unlimited_soft_is_not_low(monkeypatch: pytest.MonkeyPatch) -> None:
    """Linux spells RLIM_INFINITY as -1; unlimited must never read as low."""
    monkeypatch.setattr(sys, "platform", "linux")
    monkeypatch.setattr(
        resource, "getrlimit", lambda _which: (resource.RLIM_INFINITY, resource.RLIM_INFINITY)
    )
    monkeypatch.setattr(doctor, "_bounded_command", lambda _argv: None)
    report = _fd_limit_report()
    assert report["soft"] == resource.RLIM_INFINITY
    assert report["verdict"] == "ok"


def test_report_is_a_plain_dict(tmp_path: Path) -> None:
    """Smoke: the real report runs on this machine and keeps its shape."""
    report = _fd_limit_report()
    assert isinstance(report["soft"], int)
    assert report["threshold"] == 1024
    assert report["verdict"] in ("low", "ok")
