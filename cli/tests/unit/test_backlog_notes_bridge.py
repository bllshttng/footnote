"""`fno backlog notes`: verbatim forwarder to the native `backlog-notes` reader (x-f743)."""
from __future__ import annotations

from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()
_ENV = {"COLUMNS": "240", "NO_COLOR": "1", "TERM": "dumb"}
_FAKE_BIN = "/fake/fno-agents"


class _StubResult:
    def __init__(self, returncode):
        self.returncode = returncode


def test_notes_forwards_argv_verbatim(monkeypatch):
    import subprocess as subprocess_module

    import fno.rust_binary

    captured = {}

    def _stub_run(cmd, check=False, **kwargs):
        captured["cmd"] = list(cmd)
        return _StubResult(returncode=7)

    monkeypatch.setattr(fno.rust_binary, "resolve_binary", lambda: _FAKE_BIN)
    monkeypatch.setattr(subprocess_module, "run", _stub_run)

    result = runner.invoke(
        app, ["backlog", "notes", "history", "x-1", "--json"], env=_ENV
    )
    assert result.exit_code == 7
    assert captured["cmd"] == [_FAKE_BIN, "backlog-notes", "history", "x-1", "--json"]


def test_notes_missing_binary_exits_2_naming_the_remedy(monkeypatch):
    import fno.rust_binary

    monkeypatch.setattr(fno.rust_binary, "resolve_binary", lambda: None)
    result = runner.invoke(app, ["backlog", "notes", "history", "x-1"], env=_ENV)
    assert result.exit_code == 2
    assert "fno doctor update --rust" in result.output


def test_note_receipt_echo_survives_a_really_closed_stdout():
    """A genuinely closed pipe (read end gone, write end sitting on fd 1)
    must not raise out of the receipt echo: the note has landed by then."""
    import os as os_module
    import sys

    from fno.graph import note_cli

    read_fd, write_fd = os_module.pipe()
    os_module.close(read_fd)
    saved = os_module.dup(1)
    os_module.dup2(write_fd, 1)
    os_module.close(write_fd)
    try:
        note_cli._echo_receipt("noted x-1: the receipt that survived")
    finally:
        os_module.dup2(saved, 1)
        os_module.close(saved)
    sys.stdout.flush()
