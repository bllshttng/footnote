"""Binary-locator guards for ``fno.rust_binary``.

Two things this module can get wrong silently, both covered here:

* The ``__file__``-relative depths. The locator moved up one package level out
  of ``fno/agents/``, so ``_bundled_binary``'s ``parent`` and
  ``_cargo_dev_binary``'s ``parents[3]`` both shifted. A wrong depth returns
  ``None`` on every install instead of raising, so nothing else would notice.
* ``$FNO_AGENTS_BIN``. Both "binary not found" messages tell the operator to
  set it, and for a long time the Python resolver did not read it.
"""

from __future__ import annotations

import os
import stat
from pathlib import Path

from fno import rust_binary


def _make_exe(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("#!/bin/sh\nexit 0\n")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


def _fake_checkout(tmp_path: Path, monkeypatch) -> Path:
    """Mirror the real layout and point the module's ``__file__`` at it.

    Drives the locator's own depth arithmetic instead of restating it, so a
    wrong ``parent`` / ``parents[N]`` fails here rather than passing a test that
    recomputed the same constant.
    """
    root = tmp_path / "checkout"
    pkg = root / "cli" / "src" / "fno"
    pkg.mkdir(parents=True)
    (root / "crates").mkdir()
    monkeypatch.setattr(rust_binary, "__file__", str(pkg / "rust_binary.py"))
    return root


def test_bundled_lookup_reads_the_package_bin_dir(tmp_path, monkeypatch):
    root = _fake_checkout(tmp_path, monkeypatch)
    bundled = _make_exe(root / "cli" / "src" / "fno" / "_bin" / rust_binary.BINARY_NAME)
    assert rust_binary._bundled_binary() == bundled


def test_cargo_dev_lookup_reads_the_repo_target_dir(tmp_path, monkeypatch):
    root = _fake_checkout(tmp_path, monkeypatch)
    artifact = _make_exe(
        root / "crates" / "fno-agents" / "target" / "release" / rust_binary.BINARY_NAME
    )
    assert rust_binary._cargo_dev_binary() == artifact


def test_cargo_dev_lookup_refuses_a_non_checkout_ancestor(tmp_path, monkeypatch):
    """Installed into site-packages, the ancestor is unrelated: return None
    rather than a coincidental binary found by walking up a stranger's tree."""
    root = _fake_checkout(tmp_path, monkeypatch)
    (root / "crates").rmdir()
    _make_exe(root / "target" / "release" / rust_binary.BINARY_NAME)
    assert rust_binary._cargo_dev_binary() is None


def test_env_override_is_honored(tmp_path, monkeypatch):
    """The remedy both CLI error messages advertise actually works."""
    binary = _make_exe(tmp_path / "custom" / rust_binary.BINARY_NAME)
    monkeypatch.setenv(rust_binary.BINARY_ENV, str(binary))
    assert rust_binary.resolve_binary() == binary


def test_env_override_outranks_the_search(tmp_path, monkeypatch):
    override = _make_exe(tmp_path / "override" / rust_binary.BINARY_NAME)
    other = _make_exe(tmp_path / "onpath" / rust_binary.BINARY_NAME)
    monkeypatch.setenv("PATH", str(other.parent))
    monkeypatch.setenv(rust_binary.BINARY_ENV, str(override))
    assert rust_binary.resolve_binary() == override


def test_unusable_env_override_falls_through(tmp_path, monkeypatch):
    """A stale export must not make an installed binary unreachable."""
    on_path = _make_exe(tmp_path / "onpath" / rust_binary.BINARY_NAME)
    monkeypatch.setenv("PATH", str(on_path.parent))
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_sibling_binary", lambda: None)

    for bad in ("", "   ", str(tmp_path / "does-not-exist")):
        monkeypatch.setenv(rust_binary.BINARY_ENV, bad)
        assert rust_binary.resolve_binary() == on_path, f"failed to fall through for {bad!r}"

    # Present but not executable: also a fall-through, not a hard stop.
    not_exec = tmp_path / "plain" / rust_binary.BINARY_NAME
    not_exec.parent.mkdir(parents=True, exist_ok=True)
    not_exec.write_text("not runnable")
    not_exec.chmod(not_exec.stat().st_mode & ~stat.S_IXUSR & ~stat.S_IXGRP & ~stat.S_IXOTH)
    monkeypatch.setenv(rust_binary.BINARY_ENV, str(not_exec))
    assert rust_binary.resolve_binary() == on_path


def test_installed_lookup_ignores_the_env_override(tmp_path, monkeypatch):
    """resolve_installed_binary decides the DEFAULT runtime, so it stays deaf
    to the override; FNO_AGENTS_RUNTIME=rust is the opt-in."""
    binary = _make_exe(tmp_path / "custom" / rust_binary.BINARY_NAME)
    monkeypatch.setenv(rust_binary.BINARY_ENV, str(binary))
    monkeypatch.setenv("PATH", str(tmp_path / "empty"))
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_sibling_binary", lambda: None)
    assert rust_binary.resolve_installed_binary() is None
    assert os.environ[rust_binary.BINARY_ENV] == str(binary)


# --------------------------------------------------------------------------- #
# Resolution order. Moved here with the locator itself; these previously lived
# in test_rust_runtime.py, next to the dispatch half that no longer owns them.
# --------------------------------------------------------------------------- #

def test_installed_resolve_prefers_bundled(monkeypatch, tmp_path) -> None:
    bundled = _make_exe(tmp_path / "bundled" / rust_binary.BINARY_NAME)
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: bundled)
    monkeypatch.setattr(rust_binary, "_sibling_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_path_binary", lambda: None)
    assert rust_binary.resolve_installed_binary() == bundled


def test_installed_resolve_excludes_cargo_dev(monkeypatch, tmp_path) -> None:
    """A cargo dev artifact must NOT satisfy the installed-only resolver: a dev
    checkout stays on Python by default (the test process is never replaced)."""
    dev = _make_exe(tmp_path / "target" / "release" / rust_binary.BINARY_NAME)
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_sibling_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_path_binary", lambda: None)
    # Even if the cargo dev finder would resolve, the installed-only path ignores it.
    monkeypatch.setattr(rust_binary, "_cargo_dev_binary", lambda: dev)
    assert rust_binary.resolve_installed_binary() is None


def test_resolve_prefers_bundled(monkeypatch, tmp_path) -> None:
    bundled = _make_exe(tmp_path / "bundled" / rust_binary.BINARY_NAME)
    on_path = _make_exe(tmp_path / "path" / rust_binary.BINARY_NAME)
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: bundled)
    monkeypatch.setattr(rust_binary, "_path_binary", lambda: on_path)
    assert rust_binary.resolve_binary() == bundled


def test_resolve_falls_back_to_sibling(monkeypatch, tmp_path) -> None:
    sibling = _make_exe(tmp_path / "venvbin" / rust_binary.BINARY_NAME)
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_sibling_binary", lambda: sibling)
    monkeypatch.setattr(rust_binary, "_path_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_cargo_dev_binary", lambda: None)
    assert rust_binary.resolve_binary() == sibling


def test_resolve_falls_back_to_path(monkeypatch, tmp_path) -> None:
    on_path = _make_exe(tmp_path / "path" / rust_binary.BINARY_NAME)
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_sibling_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_path_binary", lambda: on_path)
    monkeypatch.setattr(rust_binary, "_cargo_dev_binary", lambda: None)
    assert rust_binary.resolve_binary() == on_path


def test_resolve_falls_back_to_cargo_dev(monkeypatch, tmp_path) -> None:
    dev = _make_exe(tmp_path / "target" / "release" / rust_binary.BINARY_NAME)
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_sibling_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_path_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_cargo_dev_binary", lambda: dev)
    assert rust_binary.resolve_binary() == dev


def test_resolve_returns_none_when_absent(monkeypatch) -> None:
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_sibling_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_path_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_cargo_dev_binary", lambda: None)
    assert rust_binary.resolve_binary() is None


def test_sibling_binary_finds_next_to_launcher(monkeypatch, tmp_path) -> None:
    """_sibling_binary resolves the co-installed binary via the launcher dir."""
    bindir = tmp_path / "venvbin"
    launcher = _make_exe(bindir / "fno")
    sibling = _make_exe(bindir / rust_binary.BINARY_NAME)
    monkeypatch.setattr(rust_binary.sys, "argv", [str(launcher), "agents", "ask"])
    assert rust_binary._sibling_binary() == sibling


def test_sibling_binary_none_when_absent(monkeypatch, tmp_path) -> None:
    launcher = _make_exe(tmp_path / "venvbin" / "fno")  # no fno-agents beside it
    monkeypatch.setattr(rust_binary.sys, "argv", [str(launcher)])
    assert rust_binary._sibling_binary() is None


def test_path_binary_uses_which(monkeypatch, tmp_path) -> None:
    target = _make_exe(tmp_path / rust_binary.BINARY_NAME)
    monkeypatch.setattr(rust_binary.shutil, "which", lambda name: str(target) if name == rust_binary.BINARY_NAME else None)
    assert rust_binary._path_binary() == target
