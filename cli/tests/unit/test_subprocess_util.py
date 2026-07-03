"""Unit tests for fno._subprocess_util.fno_py_cmd (x-69b3, codex P1)."""
from __future__ import annotations

from pathlib import Path

# Bind the REAL function at import time; the autouse conftest stub patches the
# module ATTRIBUTE (_subprocess_util.fno_py_cmd), not this reference, so these
# tests exercise the real resolver logic.
from fno import _subprocess_util
from fno._subprocess_util import fno_py_cmd


def test_fno_py_cmd_prefers_path(monkeypatch) -> None:
    """`fno-py` on PATH -> its resolved path (a bare-name shellout would work,
    but the absolute path is harmless and consistent)."""
    monkeypatch.setattr(_subprocess_util.shutil, "which", lambda n: "/usr/local/bin/fno-py")
    assert fno_py_cmd() == ["/usr/local/bin/fno-py"]


def test_fno_py_cmd_falls_back_to_interpreter_sibling(tmp_path: Path, monkeypatch) -> None:
    """Not on PATH -> the console script beside sys.executable, WITHOUT a PATH
    dependency. This is the codex-P1 fix: a cargo-only install (only ~/.cargo/bin
    on PATH, no ~/.local/bin) resolves fno-py via the running interpreter."""
    (tmp_path / "fno-py").write_text("#!/bin/sh\n")
    monkeypatch.setattr(_subprocess_util.shutil, "which", lambda n: None)
    monkeypatch.setattr(_subprocess_util.sys, "executable", str(tmp_path / "python"))
    assert fno_py_cmd() == [str(tmp_path / "fno-py")]


def test_fno_py_cmd_bare_name_last_resort(tmp_path: Path, monkeypatch) -> None:
    """Neither on PATH nor beside the interpreter -> the bare name, so a genuinely
    missing CLI surfaces a real subprocess error rather than a silent no-op."""
    monkeypatch.setattr(_subprocess_util.shutil, "which", lambda n: None)
    monkeypatch.setattr(_subprocess_util.sys, "executable", str(tmp_path / "python"))
    assert fno_py_cmd() == ["fno-py"]
