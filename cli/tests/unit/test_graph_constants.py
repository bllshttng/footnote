"""Tests for fno.graph._constants path resolution.

Finding 2 (P1): _constants module-level lazy attributes must route through
the typed paths accessors (paths.graph_json, paths.ledger_json, paths.briefs_dir)
so that per-resource path overrides in config.paths.* are respected.
"""
from __future__ import annotations

from pathlib import Path
from typing import Generator

import pytest


# Lazy constant names in _constants that are accessed via __getattr__.
# These can be "frozen" as real module attributes when other tests use
# monkeypatch.setattr(gc, name, value) -- the monkeypatch undo restores
# them as concrete attributes, bypassing __getattr__ entirely.
# We must delete them so our tests get fresh lazy resolution.
_LAZY_CONSTANTS = ("GRAPH_JSON", "LEDGER_JSON", "BRIEFS_DIR", "GRAPH_MD", "GRAPH_HTML", "GRAPH_ARCHIVE_JSON")


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    """Pin FNO_REPO_ROOT, clear caches, isolate settings, and flush stale module attrs."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    # Also isolate HOME so default-path resolution never escapes to the real
    # user home (important on CI where HOME=/home/runner has no settings file
    # and a polluted real home would cause the default path to leak into the
    # assertion comparison).
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    from fno import config as config_mod
    import fno.paths as paths_mod
    # Flush any stale real-attribute values that other tests may have pinned
    # onto the _constants module via monkeypatch.setattr. When monkeypatch
    # undoes a setattr on a previously-__getattr__-resolved attribute it
    # restores the attribute as a concrete module-level value, bypassing
    # __getattr__ for subsequent accesses. Deleting them here forces our
    # tests to go through __getattr__ and pick up the freshly-cleared cache.
    import fno.graph._constants as gc
    for _attr in _LAZY_CONSTANTS:
        try:
            delattr(gc, _attr)
        except AttributeError:
            pass
    yield
def _write_settings(tmp_path: Path, content: str) -> Path:
    settings_dir = tmp_path / ".fno"
    settings_dir.mkdir(parents=True, exist_ok=True)
    settings_file = settings_dir / "settings.yaml"
    settings_file.write_text(content, encoding="utf-8")
    return settings_file


# ---------------------------------------------------------------------------
# The retired paths.graph_json setting cannot move the store anchor.
# ---------------------------------------------------------------------------


def test_graph_json_constant_uses_state_dir_even_with_retired_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """GRAPH_JSON uses the state directory even if the old key is present."""
    custom_graph = tmp_path / "custom" / "my-graph.json"
    state = tmp_path / "state"
    settings_file = _write_settings(
        tmp_path,
        f"schema_version: 1\nconfig:\n  state_dir: '{state}'\n  paths:\n    graph_json: '{custom_graph}'\n",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))
    from fno import config as config_mod
    import fno.paths as paths_mod

    from fno.graph import _constants
    result = _constants.GRAPH_JSON
    assert result == state / "graph.json", (
        f"GRAPH_JSON should use the fixed state-dir anchor, got {result}"
    )


def test_ledger_json_constant_respects_paths_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP: fno.graph._constants.LEDGER_JSON uses config.paths.ledger_json when set."""
    custom_ledger = tmp_path / "custom" / "my-ledger.json"
    settings_file = _write_settings(
        tmp_path,
        f"schema_version: 1\nconfig:\n  paths:\n    ledger_json: '{custom_ledger}'\n",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))
    from fno import config as config_mod
    import fno.paths as paths_mod

    from fno.graph import _constants
    result = _constants.LEDGER_JSON
    assert result == custom_ledger, (
        f"LEDGER_JSON should respect config.paths.ledger_json override, got {result}"
    )


def test_briefs_dir_constant_respects_paths_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP: fno.graph._constants.BRIEFS_DIR uses config.paths.briefs_dir when set."""
    custom_briefs = tmp_path / "custom" / "briefs"
    settings_file = _write_settings(
        tmp_path,
        f"schema_version: 1\nconfig:\n  paths:\n    briefs_dir: '{custom_briefs}'\n",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))
    from fno import config as config_mod
    import fno.paths as paths_mod

    from fno.graph import _constants
    result = _constants.BRIEFS_DIR
    assert result == custom_briefs, (
        f"BRIEFS_DIR should respect config.paths.briefs_dir override, got {result}"
    )
