"""Tests for the a2a relay first-use confirm (ab-098967b4, US6).

Covers AC6-HP (ask once, persist), AC6-UI (prompt names ceiling + plan credit),
AC6-EDGE (no-TTY conservative fallback, NEVER inherits auto:true — F4),
AC6-FR (answered once, never re-asks), and the observed/malformed pass-through.
"""
from __future__ import annotations

import pytest
import tomllib

from fno import paths
from fno.agents import dispatch
from fno.paths_testing import use_tmpdir


class _FakeIn:
    def __init__(self, line: str, tty: bool = True):
        self._line = line
        self._tty = tty

    def isatty(self) -> bool:
        return self._tty

    def readline(self) -> str:
        return self._line


class _FakeErr:
    def __init__(self, tty: bool = True):
        self._tty = tty
        self.buf: list[str] = []

    def isatty(self) -> bool:
        return self._tty

    def write(self, s: str) -> None:
        self.buf.append(s)

    def flush(self) -> None:
        pass

    def text(self) -> str:
        return "".join(self.buf)


def _wire(monkeypatch, *, answer="y", stdin_tty=True, stderr_tty=True):
    err = _FakeErr(tty=stderr_tty)
    monkeypatch.setattr(dispatch.sys, "stdin", _FakeIn(answer, tty=stdin_tty))
    monkeypatch.setattr(dispatch.sys, "stderr", err)
    # Real confirm path (no bypass) + isolated global settings for the persist.
    monkeypatch.delenv("FNO_A2A_NO_CONFIRM", raising=False)
    return err


def test_ac6_hp_yes_keeps_on_and_persists(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "g.yaml"))
    err = _wire(monkeypatch, answer="y\n")

    assert dispatch._a2a_first_use_gate(True, 6) is True
    # AC6-UI: prompt names the ceiling + plan credit.
    assert "6" in err.text() and "plan credit" in err.text()
    # AC6-FR: marker persisted + setting written.
    assert (paths.state_dir() / ".a2a-confirmed").exists()
    assert tomllib.loads((tmp_path / "config.toml").read_text())["agents"]["a2a"]["auto"] is True


def test_ac6_hp_no_turns_off_and_persists(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "g.yaml"))
    _wire(monkeypatch, answer="n\n")

    assert dispatch._a2a_first_use_gate(True, 6) is False
    assert (paths.state_dir() / ".a2a-confirmed").exists()
    assert tomllib.loads((tmp_path / "config.toml").read_text())["agents"]["a2a"]["auto"] is False


def test_ac6_hp_empty_defaults_yes(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "g.yaml"))
    _wire(monkeypatch, answer="\n")
    assert dispatch._a2a_first_use_gate(True, 6) is True  # [Y/n] default keep-on


def test_ac6_edge_no_tty_conservative_off(tmp_path, monkeypatch):
    """F4 / Locked Decision 7: headless NEVER inherits auto:true; relay OFF,
    logged, not persisted (a later interactive run still asks)."""
    use_tmpdir(monkeypatch, tmp_path)
    err = _wire(monkeypatch, answer="y\n", stdin_tty=False, stderr_tty=False)

    assert dispatch._a2a_first_use_gate(True, 6) is False  # conservative OFF
    assert "conservative fallback" in err.text()
    # NOT persisted -> no marker, so an interactive run later still asks.
    assert not (paths.state_dir() / ".a2a-confirmed").exists()


def test_ac6_fr_marker_means_no_reask(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    marker = paths.state_dir() / ".a2a-confirmed"
    marker.parent.mkdir(parents=True, exist_ok=True)
    marker.write_text("answered\n")

    # A stdin that raises if read proves the gate did NOT ask.
    class _Boom:
        def isatty(self):
            return True

        def readline(self):
            raise AssertionError("must not re-ask once answered")

    monkeypatch.setattr(dispatch.sys, "stdin", _Boom())
    monkeypatch.setattr(dispatch.sys, "stderr", _FakeErr())
    monkeypatch.delenv("FNO_A2A_NO_CONFIRM", raising=False)
    assert dispatch._a2a_first_use_gate(True, 6) is True


def test_observed_mode_passes_through_without_ask(tmp_path, monkeypatch):
    """auto=False (incl. the malformed fail-safe) needs no confirm (AC6-ERR)."""
    use_tmpdir(monkeypatch, tmp_path)

    class _Boom:
        def isatty(self):
            return True

        def readline(self):
            raise AssertionError("must not ask in observed mode")

    monkeypatch.setattr(dispatch.sys, "stdin", _Boom())
    monkeypatch.setattr(dispatch.sys, "stderr", _FakeErr())
    monkeypatch.delenv("FNO_A2A_NO_CONFIRM", raising=False)
    assert dispatch._a2a_first_use_gate(False, 6) is False
    assert not (paths.state_dir() / ".a2a-confirmed").exists()
