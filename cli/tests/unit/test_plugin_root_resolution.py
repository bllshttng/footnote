"""Plugin-script resolution + the self-healing persisted pointer (#2).

The bug: `fno target init` / `fno gate set` could not find their plugin scripts
when run from a foreign project with no env hint. `fno` is a uv-tool install
whose wheel does not carry hooks/, and CLAUDE_PLUGIN_ROOT is not propagated to
arbitrary `fno` subprocesses, so env + package-relative both miss and the agent
had to export FNO_REPO_ROOT by hand. resolve_plugin_script adds a persisted
~/.fno/plugin-root pointer, primed on any env/pkg resolve and by the
session-start hook, as the env-less source.

The resolver reads os.environ fresh on every call (no lru_cache), so tests just
monkeypatch FNO_HOME / the plugin-root env vars - nothing to clear.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from fno import paths


def _make_plugin(root: Path) -> Path:
    (root / ".claude-plugin").mkdir(parents=True)
    (root / ".claude-plugin" / "plugin.json").write_text("{}")
    (root / "hooks" / "helpers").mkdir(parents=True)
    (root / "hooks" / "helpers" / "init-target-state.sh").write_text("#!/bin/bash\n")
    (root / "scripts" / "lib").mkdir(parents=True)
    (root / "scripts" / "lib" / "set-gate.sh").write_text("#!/bin/bash\n")
    return root


@pytest.fixture
def isolated_home(tmp_path, monkeypatch):
    """FNO_HOME -> tmp dir; all plugin-root env hints cleared."""
    home = tmp_path / "abi-home"
    monkeypatch.setenv("FNO_HOME", str(home))
    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    monkeypatch.delenv("CONDUCTOR_ROOT_PATH", raising=False)
    return home


def test_persist_gated_on_manifest(tmp_path, isolated_home):
    """A root without the plugin manifest is never written (no test poisoning)."""
    no_manifest = tmp_path / "fake"
    (no_manifest / "hooks" / "helpers").mkdir(parents=True)
    (no_manifest / "hooks" / "helpers" / "init-target-state.sh").write_text("x")
    paths._persist_plugin_root(no_manifest)
    assert not (isolated_home / "plugin-root").exists()

    plugin = _make_plugin(tmp_path / "plugin")
    paths._persist_plugin_root(plugin)
    assert (isolated_home / "plugin-root").read_text().strip() == str(plugin)


def test_read_persisted_returns_none_when_stale(tmp_path, isolated_home):
    isolated_home.mkdir(parents=True, exist_ok=True)
    (isolated_home / "plugin-root").write_text(str(tmp_path / "gone") + "\n")
    assert paths._read_persisted_plugin_root() is None


def test_env_hint_resolves_and_self_heals_pointer(tmp_path, monkeypatch, isolated_home):
    plugin = _make_plugin(tmp_path / "plugin")
    monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", str(plugin))
    got = paths.resolve_plugin_script("scripts/lib/set-gate.sh")
    assert got == plugin / "scripts" / "lib" / "set-gate.sh"
    # env resolve primed the pointer for later env-less runs
    assert (isolated_home / "plugin-root").read_text().strip() == str(plugin)


def test_resolve_falls_to_persisted_when_env_and_pkg_miss(tmp_path, monkeypatch, isolated_home):
    """The env-less foreign-project case: only the persisted pointer remains."""
    plugin = _make_plugin(tmp_path / "plugin")
    isolated_home.mkdir(parents=True, exist_ok=True)
    (isolated_home / "plugin-root").write_text(str(plugin) + "\n")
    # Force package-relative (the in-tree repo root) to NOT count as a plugin,
    # so resolution must reach the persisted pointer.
    monkeypatch.setattr(paths, "_is_plugin_root", lambda r: Path(r) == plugin)
    got = paths.resolve_plugin_script("scripts/lib/set-gate.sh")
    assert got == plugin / "scripts" / "lib" / "set-gate.sh"


def _git(cwd: Path, *args: str) -> None:
    subprocess.run(["git", "-C", str(cwd), *args], check=True,
                   capture_output=True, text=True)


def _make_worktree_plugin(tmp_path: Path) -> tuple[Path, Path]:
    """A canonical git checkout carrying the plugin + one linked worktree that
    also carries it (mirrors how every footnote worktree shares tracked files).
    Returns (canonical_root, worktree_root)."""
    canon = _make_plugin(tmp_path / "canon")
    _git(canon, "init", "-q")
    _git(canon, "-c", "user.email=t@t", "-c", "user.name=t",
         "add", "-A")
    _git(canon, "-c", "user.email=t@t", "-c", "user.name=t",
         "commit", "-q", "-m", "init")
    wt = tmp_path / "wt"
    _git(canon, "worktree", "add", "-q", str(wt))
    return canon, wt


def test_worktree_root_canonicalizes_to_main(tmp_path):
    """The core cold-start-receipt fix (x-9d3c): a worktree plugin root maps to
    its canonical checkout so resolution never runs a foreign worktree's script
    (the source of Usage:/command-not-found noise). A non-worktree root is a
    no-op."""
    canon, wt = _make_worktree_plugin(tmp_path)
    assert paths._canonical_plugin_root(wt).resolve() == canon.resolve()
    assert paths._canonical_plugin_root(canon).resolve() == canon.resolve()
    # Non-git dir returns unchanged (OSS --plugin-dir tarball).
    plain = _make_plugin(tmp_path / "plain")
    assert paths._canonical_plugin_root(plain) == plain


def test_persisted_worktree_pointer_self_heals_to_canonical(tmp_path, isolated_home):
    """The session-start hook writes the CURRENT worktree to the pointer; the
    reader canonicalizes it so resolution runs the canonical init script and a
    cold-start receipt stays clean. This is the sole canonicalization point."""
    canon, wt = _make_worktree_plugin(tmp_path)
    isolated_home.mkdir(parents=True, exist_ok=True)
    (isolated_home / "plugin-root").write_text(str(wt) + "\n")
    got = paths._read_persisted_plugin_root()
    assert got is not None and got.resolve() == canon.resolve()


def test_canonical_fails_open_on_subprocess_error(tmp_path, monkeypatch):
    """A hanging/failing git probe (timeout, missing git) or a stubbed run
    without .stdout must fail open to the input root, never crash resolution."""
    plugin = _make_plugin(tmp_path / "plugin")

    def _timeout(*a, **k):
        raise subprocess.TimeoutExpired(cmd="git", timeout=2)
    monkeypatch.setattr(paths.subprocess, "run", _timeout)
    assert paths._canonical_plugin_root(plugin) == plugin

    class _NoStdout:
        returncode = 0
    monkeypatch.setattr(paths.subprocess, "run", lambda *a, **k: _NoStdout())
    assert paths._canonical_plugin_root(plugin) == plugin


def test_persist_stays_subprocess_free(tmp_path, isolated_home, monkeypatch):
    """Persisting must not shell out (a git call here would trip the target-init
    'must not shell out' guards): it stores the raw root; the reader canonicalizes."""
    canon, wt = _make_worktree_plugin(tmp_path)  # git setup BEFORE the boom patch

    def _boom(*a, **k):
        raise AssertionError("persist must not shell out")
    monkeypatch.setattr(paths.subprocess, "run", _boom)
    paths._persist_plugin_root(wt)  # would raise if it shelled out
    assert (isolated_home / "plugin-root").read_text().strip() == str(wt)


