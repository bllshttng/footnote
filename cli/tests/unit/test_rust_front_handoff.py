"""The fno-py to Rust-front hand-off refuses to recurse.

A PATH `fno` that forwards to fno-py re-enters `_run_rust_front`; the
FNO_PY_HANDOFF marker turns the second entry into a named refusal instead
of the 2026-10-02 stack (~9,000 processes, 89 GB resident).
"""

import shutil

import pytest
import typer

from fno.cli import _run_rust_front


def test_recursive_handoff_refuses_with_marker_set(monkeypatch, capsys):
    monkeypatch.setenv("FNO_PY_HANDOFF", "1")
    with pytest.raises(typer.Exit) as exc:
        _run_rust_front(["mux"])
    assert exc.value.exit_code == 1
    captured = capsys.readouterr()
    assert "FNO_PY_HANDOFF" in captured.err
    assert "recursive" in captured.err


def test_handoff_sets_marker_for_the_child(monkeypatch, tmp_path):
    dump = tmp_path / "dump.txt"
    script = tmp_path / "fake-fno"
    script.write_text(f"#!/bin/sh\nprintenv FNO_PY_HANDOFF > {dump}\n")
    script.chmod(0o755)
    monkeypatch.setattr(shutil, "which", lambda name: str(script))
    with pytest.raises(typer.Exit) as exc:
        _run_rust_front(["version"])
    assert exc.value.exit_code == 0
    assert dump.read_text().strip() == "1"
