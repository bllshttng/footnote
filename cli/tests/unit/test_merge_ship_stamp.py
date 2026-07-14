"""x-b6e4: `fno pr merge` stamps ship-phase lifecycle provenance on a real merge."""
from __future__ import annotations

import json
from pathlib import Path


def _make_graph(tmp_path: Path, entries: list[dict]) -> Path:
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")
    return g


def _patch(monkeypatch, g: Path) -> None:
    import fno.graph._constants as gc
    import fno.graph.store as gs
    import fno.paths as paths
    lock = g.parent / "graph.lock"
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", g.parent / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", lock)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", lock)
    monkeypatch.setattr(paths, "graph_json", lambda: g)


def _clear_env(monkeypatch):
    for v in ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID"):
        monkeypatch.delenv(v, raising=False)


def _sessions(g: Path, node_id: str) -> list[dict]:
    from fno.graph.store import read_graph
    return next(e for e in read_graph(g) if e["id"] == node_id).get("sessions", [])


def test_merged_stamps_ship(tmp_path, monkeypatch):
    g = _make_graph(tmp_path, [{"id": "ab-mrg00001", "title": "t", "pr_number": 4242}])
    _patch(monkeypatch, g)
    _clear_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "merger-sid")

    from fno.pr._merge import _sync_graph_merge_status
    _sync_graph_merge_status("merged", 4242)

    rows = _sessions(g, "ab-mrg00001")
    assert len(rows) == 1
    assert (rows[0]["phase"], rows[0]["session_id"]) == ("ship", "merger-sid")


def test_queued_does_not_stamp_ship(tmp_path, monkeypatch):
    """Auto-merge queued (not yet merged) must NOT record a ship entry."""
    g = _make_graph(tmp_path, [{"id": "ab-mrg00002", "title": "t", "pr_number": 4343}])
    _patch(monkeypatch, g)
    _clear_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "merger-sid")

    from fno.pr._merge import _sync_graph_merge_status
    _sync_graph_merge_status("queued", 4343)

    assert _sessions(g, "ab-mrg00002") == []


def test_merged_no_identity_skips_silently(tmp_path, monkeypatch):
    g = _make_graph(tmp_path, [{"id": "ab-mrg00003", "title": "t", "pr_number": 4444}])
    _patch(monkeypatch, g)
    _clear_env(monkeypatch)  # no ambient identity

    from fno.pr._merge import _sync_graph_merge_status
    _sync_graph_merge_status("merged", 4444)  # must not raise

    assert _sessions(g, "ab-mrg00003") == []


def test_merged_stamps_scoped_by_repo(tmp_path, monkeypatch):
    """x-d5f9: the merge stamp scopes by the merging repo's slug, so a
    same-numbered PR in another repo is never stamped. The slug is injected
    (in-test gh is unauthed under the hermetic HOME, so it would degrade to
    None); this asserts the threading + narrowing deterministically."""
    g = _make_graph(tmp_path, [
        {"id": "x-foot0388", "title": "footnote", "pr_number": 388,
         "pr_url": "https://github.com/bllshttng/footnote/pull/388"},
        {"id": "ab-abil0388", "title": "abilities", "pr_number": 388,
         "pr_url": "https://github.com/bllshttng/abilities/pull/388"},
    ])
    _patch(monkeypatch, g)
    _clear_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "merger-sid")

    import fno.pr._merge as M
    monkeypatch.setattr(M, "_repo_slug", lambda cwd: "bllshttng/footnote")
    M._sync_graph_merge_status("merged", 388, "/some/worktree")

    assert [r["phase"] for r in _sessions(g, "x-foot0388")] == ["ship"]
    assert _sessions(g, "ab-abil0388") == []  # other repo never stamped
