"""The ONE render path: the keeper's render trigger owns view rendering.

A store write bumps the store's counter, the canonical keeper's render
thread notices, settles 2 s, and replays the canonical view pass once
per write burst. These tests pin the Python contract the trigger relies
on: the write path no longer renders inline, the view pass renders the
configured targets from a fresh read, and a failing target never
blocks the next write.
"""

from pathlib import Path

import pytest

from fno.graph.store import locked_mutate_graph, render_canonical_views


@pytest.fixture
def paths(tmp_path, monkeypatch):
    """Pin HOME, the graph constants, and the global config so a test never
    reads or writes the operator's real store, targets, or vault."""
    home = tmp_path / "home"
    home.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path / "repo"))
    monkeypatch.delenv("FNO_CONFIG", raising=False)

    import fno.graph._constants as gc

    for attr in ("GRAPH_JSON", "GRAPH_MD", "GRAPH_HTML"):
        try:
            delattr(gc, attr)
        except AttributeError:
            pass

    graph = tmp_path / "graph.json"
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setitem(vars(gc), "GRAPH_JSON", graph)
    monkeypatch.setitem(vars(gc), "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setitem(vars(gc), "GRAPH_HTML", tmp_path / "graph.html")
    return {"graph": graph, "target": tmp_path / "out" / "board.html"}


def _config(monkeypatch, target):
    """Point the global config at one local-projection render target."""
    cfg = target.parent.parent / "config.toml"
    cfg.write_text(
        "[backlog]\n"
        "render_targets = [{path = '%s', projection = 'local'}]\n" % target,
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(cfg))
    from fno.config import _load_settings_at

    _load_settings_at.cache_clear()


def _seed(graph, title):
    """One store write: the node the later assertions look for."""
    locked_mutate_graph(
        graph,
        lambda entries: [
            {
                "id": "x-rend",
                "slug": "x-rend",
                "title": title,
                "type": "feature",
                "status": "idea",
                "priority": "p2",
                "created_at": "2026-09-11T00:00:00+00:00",
            }
        ],
    )


def test_write_path_renders_the_configured_store_inline(paths, monkeypatch):
    """A write against the configured store renders the configured targets
    in the same call, so CLI consumers read a fresh board without waiting
    out the trigger's settle. The trigger exists for the writers that bypass
    this client (mux native ops, Rust mutations) and to retry failures."""
    graph = paths["graph"]
    target = paths["target"]
    _config(monkeypatch, target)
    _seed(graph, "Render follows write inline")

    assert graph.exists(), "the store write landed"
    assert "Render follows write inline" in target.read_text(encoding="utf-8")


def test_render_pass_renders_configured_targets(paths, monkeypatch):
    """The pass the trigger invokes renders every configured target from a
    fresh store read, so a write's change reaches the target on the next
    settled tick (AC17-HP's budget, asserted without a sleep here)."""
    graph = paths["graph"]
    target = paths["target"]
    _config(monkeypatch, target)
    _seed(graph, "Render follows write")

    render_canonical_views()

    text = target.read_text(encoding="utf-8")
    assert "Render follows write" in text


def test_failing_target_never_blocks_the_next_write(paths, monkeypatch):
    """A render target that fails leaves the writes untouched: the next
    write lands and the pass runs again without raising (AC17-HP)."""
    graph = paths["graph"]
    target = paths["target"]
    _config(monkeypatch, target)
    _seed(graph, "First write")
    render_canonical_views()

    # Break the target: its parent becomes a file, so the atomic write fails.
    target.unlink()
    target.parent.rmdir()
    target.parent.write_text("not a directory")
    render_canonical_views()  # must not raise

    _seed(graph, "Second write")
    render_canonical_views()  # must not raise either
    assert "Second write" in graph.read_text(encoding="utf-8")
