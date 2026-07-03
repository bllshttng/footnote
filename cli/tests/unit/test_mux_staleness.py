"""Unit tests for the running-mux-server staleness detector (x-e6dd).

`stale_mux_servers()` flags live mux sessions whose SERVER predates the installed
mux binary (socket mtime < binary mtime) - a long-running server still speaking
the old proto after an upgrade, fixed by `fno restart --mux`. Proto-agnostic and
best-effort. The mux binary + live-session enumeration are monkeypatched; socket
mtimes are set explicitly so the mtime comparison is exercised for real.
"""
from __future__ import annotations

import os
from pathlib import Path
from types import SimpleNamespace

from fno import update


def _touch(p: Path, mtime: float) -> None:
    p.write_text("x")
    os.utime(p, (mtime, mtime))


def test_flags_only_sockets_older_than_the_binary(monkeypatch, tmp_path):
    mux_dir = tmp_path / "mux"
    mux_dir.mkdir()
    monkeypatch.setenv("FNO_MUX_DIR", str(mux_dir))
    binary = tmp_path / "fno"
    _touch(binary, 1000.0)
    monkeypatch.setattr(update, "_cargo_installed_mux", lambda: binary)
    monkeypatch.setattr(update, "_live_mux_sessions", lambda: ["stale", "fresh", "gone"])
    _touch(mux_dir / "stale.sock", 500.0)  # server started before the binary -> stale
    _touch(mux_dir / "fresh.sock", 1500.0)  # server started after the binary -> fresh
    # "gone" has no socket on disk -> skipped, never flagged
    assert update.stale_mux_servers() == ["stale"]


def test_empty_when_no_installed_mux_binary(monkeypatch):
    monkeypatch.setattr(update, "_cargo_installed_mux", lambda: None)
    monkeypatch.setattr(update, "_live_mux_sessions", lambda: ["x"])
    assert update.stale_mux_servers() == []


def test_live_mux_sessions_parses_live_only(monkeypatch, tmp_path):
    binary = tmp_path / "fno"
    binary.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_mux", lambda: binary)

    def fake_run(cmd, **kw):
        return SimpleNamespace(
            returncode=0,
            stdout='[{"session":"a","state":"live"},'
            '{"session":"b","state":"stale"},'
            '{"session":"c","state":"live"}]',
        )

    assert update._live_mux_sessions(runner=fake_run) == ["a", "c"]


def test_live_mux_sessions_empty_on_error_or_bad_json(monkeypatch, tmp_path):
    binary = tmp_path / "fno"
    binary.write_text("x")
    monkeypatch.setattr(update, "_cargo_installed_mux", lambda: binary)

    assert update._live_mux_sessions(
        runner=lambda cmd, **kw: SimpleNamespace(returncode=1, stdout="")
    ) == []
    assert update._live_mux_sessions(
        runner=lambda cmd, **kw: SimpleNamespace(returncode=0, stdout="not json")
    ) == []
