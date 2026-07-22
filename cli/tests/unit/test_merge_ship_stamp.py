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
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", g.parent / "graph.md")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(paths, "graph_json", lambda: g)


def _clear_env(monkeypatch):
    for v in ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID"):
        monkeypatch.delenv(v, raising=False)


def _sessions(g: Path, node_id: str) -> list[dict]:
    from fno.graph.store import read_graph
    return next(e for e in read_graph(g) if e["id"] == node_id).get("sessions", [])


def test_merged_stamps_ship(tmp_path, monkeypatch):
    g = _make_graph(tmp_path, [{"id": "ab-mrg00001", "title": "t", "pr_number": 4242,
                                "pr_url": "https://github.com/bllshttng/footnote/pull/4242"}])
    _patch(monkeypatch, g)
    _clear_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "merger-sid")

    import fno.pr._merge as M
    monkeypatch.setattr(M, "_repo_slug", lambda cwd: "bllshttng/footnote")
    M._sync_graph_merge_status("merged", 4242)

    rows = _sessions(g, "ab-mrg00001")
    assert len(rows) == 1
    assert (rows[0]["phase"], rows[0]["session_id"]) == ("ship", "merger-sid")


def test_merged_skips_ship_when_repo_unresolved(tmp_path, monkeypatch):
    """codex P2: an unresolved repo slug must SKIP, not fall back to a bare
    pr_number match that could stamp a same-numbered PR in another repo."""
    g = _make_graph(tmp_path, [{"id": "ab-mrg00009", "title": "t", "pr_number": 4242}])
    _patch(monkeypatch, g)
    _clear_env(monkeypatch)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "merger-sid")

    import fno.pr._merge as M
    monkeypatch.setattr(M, "_repo_slug", lambda cwd: None)  # gh flake / misconfig
    M._sync_graph_merge_status("merged", 4242)

    assert _sessions(g, "ab-mrg00009") == []  # skipped, not stamped on a bare match


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


# --- fno pr merge closes its own node (baked-in reconcile, no memory) ---------


def _fake_gh_url(url: str):
    class _R:
        ok = True
        stdout = url
        stderr = ""
    return lambda args, cwd: _R()


def test_find_pr_node_id_by_url_when_number_absent():
    import fno.pr._merge as M
    entries = [{"id": "ab-x", "pr_url": "https://x/pull/5"}]
    assert M._find_pr_node_id(entries, 5, "https://x/pull/5") == "ab-x"
    # no int pr_number AND no url to match on -> unresolved (not a wrong guess)
    assert M._find_pr_node_id(entries, 5, "") is None


def test_reconcile_merged_pr_node_stamps_number_and_closes(tmp_path, monkeypatch):
    # A node linked ONLY by pr_url (no pr_number) is invisible to bare reconcile
    # (forward scan needs an int number; reverse map needs the id in the branch).
    # `fno pr merge` must still find it, stamp the number, and run the scoped close.
    url = "https://github.com/bllshttng/footnote/pull/777"
    g = _make_graph(tmp_path, [{"id": "ab-recon001", "title": "t", "pr_url": url}])
    _patch(monkeypatch, g)
    import fno.pr._merge as M
    monkeypatch.setattr(M, "_gh", _fake_gh_url(url))
    calls: list[list[str]] = []
    monkeypatch.setattr(M, "run", lambda argv, cwd=None: calls.append(argv))

    M._reconcile_merged_pr_node(777, cwd=str(tmp_path))

    from fno.graph.store import read_graph
    node = next(e for e in read_graph(g) if e["id"] == "ab-recon001")
    assert node["pr_number"] == 777  # canonical link now stamped
    assert len(calls) == 1
    assert calls[0][-4:] == ["backlog", "reconcile", "--node", "ab-recon001"]


def test_reconcile_merged_pr_node_noop_without_matching_node(tmp_path, monkeypatch):
    g = _make_graph(tmp_path, [{"id": "ab-other01", "title": "t", "pr_number": 1}])
    _patch(monkeypatch, g)
    import fno.pr._merge as M
    monkeypatch.setattr(
        M, "_gh", _fake_gh_url("https://github.com/bllshttng/footnote/pull/999")
    )
    calls: list[list[str]] = []
    monkeypatch.setattr(M, "run", lambda argv, cwd=None: calls.append(argv))

    M._reconcile_merged_pr_node(999, cwd=str(tmp_path))

    assert calls == []  # no node for PR #999 -> nothing closed


def test_on_confirmed_merge_syncs_status_and_closes_node(tmp_path, monkeypatch):
    # The single merged-path choke: sync merge_status AND close the node.
    url = "https://github.com/bllshttng/footnote/pull/555"
    g = _make_graph(tmp_path, [{"id": "ab-conf001", "title": "t", "pr_number": 555,
                                "pr_url": url}])
    _patch(monkeypatch, g)
    _clear_env(monkeypatch)
    import fno.pr._merge as M
    monkeypatch.setattr(M, "_gh", _fake_gh_url(url))
    monkeypatch.setattr(M, "_repo_slug", lambda cwd: "bllshttng/footnote")
    calls: list[list[str]] = []
    monkeypatch.setattr(M, "run", lambda argv, cwd=None: calls.append(argv))

    M._on_confirmed_merge(555, str(tmp_path))

    from fno.graph.store import read_graph
    node = next(e for e in read_graph(g) if e["id"] == "ab-conf001")
    assert node.get("merge_status") == "merged"          # sync ran
    assert calls and calls[0][-4:] == ["backlog", "reconcile", "--node", "ab-conf001"]  # close ran
