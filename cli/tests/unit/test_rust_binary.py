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


# --------------------------------------------------------------------------- #
# Freshness. Four resolvers picked a build artifact by profile NAME in two
# opposite orders, so a stale debug build shadowed a fresh release beside it;
# `newest_runnable` decides by mtime instead and every resolver routes
# through it.
# --------------------------------------------------------------------------- #

def test_newest_runnable_picks_the_newer_whichever_order(tmp_path):
    old = _make_exe(tmp_path / "old" / "bin")
    new = _make_exe(tmp_path / "new" / "bin")
    os.utime(old, ns=(1_000_000_000, 1_000_000_000))
    assert rust_binary.newest_runnable([old, new]) == new
    assert rust_binary.newest_runnable([new, old]) == new


def test_newest_runnable_skips_unrunnable_and_answers_none(tmp_path):
    missing = tmp_path / "gone" / "bin"
    plain = tmp_path / "plain" / "bin"
    plain.parent.mkdir(parents=True)
    plain.write_text("not runnable")
    assert rust_binary.newest_runnable([missing, plain]) is None
    assert rust_binary.newest_runnable([]) is None


def test_newest_runnable_tie_keeps_the_first_candidate(tmp_path):
    """max() returns the FIRST maximal element, so equal mtimes keep the
    caller's existing preference and the result repeats across runs."""
    first = _make_exe(tmp_path / "a" / "bin")
    second = _make_exe(tmp_path / "b" / "bin")
    os.utime(first, ns=(1_000_000_000, 1_000_000_000))
    os.utime(second, ns=(1_000_000_000, 1_000_000_000))
    assert rust_binary.newest_runnable([first, second]) is first


def test_cargo_dev_lookup_prefers_the_newest_build(tmp_path, monkeypatch):
    """A fresh release must beat a stale debug; the mtimes inverted, a fresh
    debug must win. Never the profile name."""
    root = _fake_checkout(tmp_path, monkeypatch)
    debug = _make_exe(root / "crates" / "fno-agents" / "target" / "debug" / rust_binary.BINARY_NAME)
    release = _make_exe(root / "crates" / "fno-agents" / "target" / "release" / rust_binary.BINARY_NAME)
    os.utime(debug, ns=(1_000_000_000, 1_000_000_000))
    assert rust_binary._cargo_dev_binary() == release
    os.utime(release, ns=(0, 0))
    assert rust_binary._cargo_dev_binary() == debug


def test_find_dev_binary_prefers_the_newest_and_answers_none_when_absent(tmp_path, monkeypatch):
    root = _fake_checkout(tmp_path, monkeypatch)
    (root / "crates" / "fno-agents").mkdir()
    assert rust_binary.find_dev_binary() is None  # the @requires_rust skip stays
    debug = _make_exe(root / "crates" / "fno-agents" / "target" / "debug" / rust_binary.BINARY_NAME)
    release = _make_exe(root / "crates" / "fno-agents" / "target" / "release" / rust_binary.BINARY_NAME)
    assert rust_binary.find_dev_binary() == release
    os.utime(release, ns=(0, 0))
    assert rust_binary.find_dev_binary() == debug


def test_worker_binary_lets_path_compete_on_freshness(tmp_path, monkeypatch):
    """The store's resolver used to answer the first checkout artifact and
    never reached PATH; a stale debug build beat a fresh installed one."""
    from fno.graph import store

    root = _fake_checkout(tmp_path, monkeypatch)
    (root / "crates" / "fno-agents").mkdir()
    stale = _make_exe(root / "crates" / "fno-agents" / "target" / "debug" / "fno-agents-worker")
    os.utime(stale, ns=(0, 0))
    fresh = _make_exe(tmp_path / "onpath" / "fno-agents-worker")
    monkeypatch.delenv("FNO_AGENTS_WORKER", raising=False)
    monkeypatch.delenv("FNO_AGENTS_FRONT", raising=False)
    monkeypatch.setattr(store.shutil, "which", lambda name: str(fresh))
    assert store._worker_binary() == fresh


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


def test_front_env_is_honored_when_path_has_none(tmp_path, monkeypatch):
    """The smoke lanes export only FNO_AGENTS_FRONT for the built binary.

    The footprint door resolves through resolve_binary, so a lane that built
    the binary and exported the FRONT name must stay readable; a stale FRONT
    export must still lose to a fresh install on PATH."""
    front = _make_exe(tmp_path / "built" / rust_binary.BINARY_NAME)
    monkeypatch.setenv("FNO_AGENTS_FRONT", str(front))
    monkeypatch.delenv(rust_binary.BINARY_ENV, raising=False)
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_sibling_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_cargo_dev_binary", lambda: None)
    monkeypatch.delenv("PATH", raising=False)
    assert rust_binary.resolve_binary() == front

    on_path = _make_exe(tmp_path / "onpath" / rust_binary.BINARY_NAME)
    monkeypatch.setenv("PATH", str(on_path.parent))
    assert rust_binary.resolve_binary() == on_path

    monkeypatch.delenv("PATH", raising=False)
    for bad in ("", "   ", str(tmp_path / "gone")):
        monkeypatch.setenv("FNO_AGENTS_FRONT", bad)
        # Every other finder is stubbed out, so a junk FRONT is a fall-through
        # to nothing: None, never the stale value.
        assert rust_binary.resolve_binary() is None, f"junk FRONT won for {bad!r}"


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
    monkeypatch.setattr(rust_binary, "_env_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: bundled)
    monkeypatch.setattr(rust_binary, "_path_binary", lambda: on_path)
    assert rust_binary.resolve_binary() == bundled


def test_resolve_falls_back_to_sibling(monkeypatch, tmp_path) -> None:
    sibling = _make_exe(tmp_path / "venvbin" / rust_binary.BINARY_NAME)
    monkeypatch.setattr(rust_binary, "_env_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_sibling_binary", lambda: sibling)
    monkeypatch.setattr(rust_binary, "_path_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_cargo_dev_binary", lambda: None)
    assert rust_binary.resolve_binary() == sibling


def test_resolve_falls_back_to_path(monkeypatch, tmp_path) -> None:
    on_path = _make_exe(tmp_path / "path" / rust_binary.BINARY_NAME)
    monkeypatch.setattr(rust_binary, "_env_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_sibling_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_path_binary", lambda: on_path)
    monkeypatch.setattr(rust_binary, "_cargo_dev_binary", lambda: None)
    assert rust_binary.resolve_binary() == on_path


def test_resolve_falls_back_to_cargo_dev(monkeypatch, tmp_path) -> None:
    dev = _make_exe(tmp_path / "target" / "release" / rust_binary.BINARY_NAME)
    monkeypatch.setattr(rust_binary, "_env_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_sibling_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_path_binary", lambda: None)
    # The smoke lanes export FRONT for the built binary; this leg is below it.
    monkeypatch.delenv("FNO_AGENTS_FRONT", raising=False)
    monkeypatch.setattr(rust_binary, "_cargo_dev_binary", lambda: dev)
    assert rust_binary.resolve_binary() == dev


def test_resolve_returns_none_when_absent(monkeypatch) -> None:
    monkeypatch.setattr(rust_binary, "_env_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_bundled_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_sibling_binary", lambda: None)
    monkeypatch.setattr(rust_binary, "_path_binary", lambda: None)
    monkeypatch.delenv("FNO_AGENTS_FRONT", raising=False)
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


def test_verb_call_attaches_returncode(monkeypatch) -> None:
    """A clean non-zero exit carries its code as a structured field.

    The gh budget's admit gate refuses on a signalled reader (negative code or
    137) and admits on a clean failure, so the code must be readable as data,
    not only as prose in the message.
    """
    import subprocess

    import pytest

    from fno.rust_binary import VerbUnavailable

    proc = subprocess.CompletedProcess(
        ["fno-agents", "fleet-incident"], returncode=1, stdout="", stderr="no ledger"
    )
    monkeypatch.setattr(rust_binary, "find_dev_binary", lambda: "/dev/null/fno-agents")
    monkeypatch.setattr(subprocess, "run", lambda *a, **k: proc)
    with pytest.raises(VerbUnavailable) as excinfo:
        rust_binary.verb_call("fleet-incident", {})
    assert excinfo.value.returncode == 1
    assert "exited 1" in str(excinfo.value)


def test_verb_call_attaches_returncode_with_passthrough_stderr(monkeypatch) -> None:
    """The passthrough_stderr failure path carries the code too.

    The stream-it-live callers (spawn gates) raise on the same non-zero exit;
    a consumer discriminating a signal kill must read None only when there
    truly was no exit, never because the stderr went to the terminal.
    """
    import subprocess

    import pytest

    from fno.rust_binary import VerbUnavailable

    proc = subprocess.CompletedProcess(
        ["fno-agents", "spawn-axes"], returncode=-9, stdout="", stderr=""
    )
    monkeypatch.setattr(rust_binary, "find_dev_binary", lambda: "/dev/null/fno-agents")
    monkeypatch.setattr(subprocess, "run", lambda *a, **k: proc)
    with pytest.raises(VerbUnavailable) as excinfo:
        rust_binary.verb_call("spawn-axes", {}, passthrough_stderr=True)
    assert excinfo.value.returncode == -9
    assert "exited -9" in str(excinfo.value)
