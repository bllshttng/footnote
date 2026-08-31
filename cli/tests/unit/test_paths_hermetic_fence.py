from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

import pytest

from fno import paths


def _settings(state_dir: str, graph_json: str | None = None):
    return SimpleNamespace(
        state_dir=state_dir,
        paths=SimpleNamespace(graph_json=graph_json),
    )


def test_graph_json_refuses_a_resolved_path_outside_the_sandbox(
    monkeypatch, tmp_path: Path
):
    outside = Path("/var/tmp/fno-hermetic-escape") / "graph.json"
    monkeypatch.setenv("FNO_TEST_HERMETIC", "1")
    monkeypatch.setattr(paths, "_settings", lambda: _settings(str(outside.parent)))

    with pytest.raises(RuntimeError, match="fno-hermetic-escape|sandbox|explicit") as exc:
        paths.graph_json()

    assert str(outside.parent) in str(exc.value)
    assert "explicit" in str(exc.value)


def test_graph_json_allows_a_resolved_path_inside_the_sandbox(
    monkeypatch, tmp_path: Path
):
    inside = tmp_path / "state"
    monkeypatch.setenv("FNO_TEST_HERMETIC", "1")
    monkeypatch.setattr(paths, "_settings", lambda: _settings(str(inside)))

    assert paths.graph_json() == inside / "graph.json"


def test_locks_dir_remains_home_anchored_under_the_fence(monkeypatch):
    monkeypatch.setenv("FNO_TEST_HERMETIC", "1")

    assert paths.locks_dir() == Path.home() / ".fno" / "locks"
