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


