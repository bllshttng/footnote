"""`fno do pr merge` reconcile and node-resolution coverage.

The ship-phase provenance stamp that gave this file its name moved to
`fno backlog update --pr-number` (the PR-link choke point every shipped node
passes through). What remains here is the coverage the deletion nearly took
with it: the codex P1/P2 cross-repo collision guards on `_find_pr_node_id`
(unused by `_reconcile_merged_pr_node` since x-59a6, but kept as a tested
utility), and the repo-scoped delegation / unsuppressed-failure behavior of
`_reconcile_merged_pr_node` and `_on_confirmed_merge`. Those two now
delegate entirely to `backlog reconcile --pr-number/--repo` (x-59a6) rather
than resolving and stamping one node by hand, so a PR naming several nodes
closes all of them, not just the one this process happens to find first;
the plural binding itself is tested in test_pr_closure.py and
test_backlog_reconcile.py. These functions still run on every confirmed
merge, so losing the tests here would let a future refactor regress the
repo-scoping-is-mandatory guarantee they pin.
"""
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


# --- fno do pr merge closes its own node (baked-in reconcile, no memory) ---------

_FOOT = "https://github.com/bllshttng/footnote/pull"


def _stub_run(calls, *, ok=True, stderr=""):
    """A run() stub that records argv and returns a Result-like object."""
    class _R:
        def __init__(self):
            self.ok = ok
            self.stdout = ""
            self.stderr = stderr

    def _run(argv, cwd=None):
        calls.append(argv)
        return _R()

    return _run


def _fake_gh_url(url: str):
    class _R:
        ok = True
        stdout = url
        stderr = ""
    return lambda args, cwd: _R()


def test_find_pr_node_id_by_exact_url():
    import fno.pr._merge as M
    entries = [{"id": "ab-x", "pr_url": f"{_FOOT}/5"}]
    assert M._find_pr_node_id(entries, 5, f"{_FOOT}/5") == "ab-x"


def test_find_pr_node_id_refuses_bare_number_without_url():
    # No url to scope against -> a bare-number match is cross-repo-unsafe -> refuse.
    import fno.pr._merge as M
    entries = [{"id": "ab-x", "pr_number": 5}]
    assert M._find_pr_node_id(entries, 5, "") is None


def test_find_pr_node_id_refuses_cross_repo_number_collision():
    # Two repos, same PR number; our url is footnote -> return footnote's node,
    # never the abilities node (codex P1: a bare-number match closed unrelated work).
    import fno.pr._merge as M
    entries = [
        {"id": "ab-abil", "pr_number": 5,
         "pr_url": "https://github.com/bllshttng/abilities/pull/5"},
        {"id": "ab-foot", "pr_number": 5, "pr_url": f"{_FOOT}/5"},
    ]
    assert M._find_pr_node_id(entries, 5, f"{_FOOT}/5") == "ab-foot"


def test_find_pr_node_id_number_scoped_to_our_repo():
    # No exact-url node; a number match must be scoped to our repo, so only the
    # footnote node qualifies - the abilities same-number node is rejected.
    import fno.pr._merge as M
    entries = [
        {"id": "ab-abil", "pr_number": 9,
         "pr_url": "https://github.com/bllshttng/abilities/pull/9"},
        {"id": "ab-foot", "pr_number": 9, "pr_url": f"{_FOOT}/9"},
    ]
    assert M._find_pr_node_id(entries, 9, f"{_FOOT}/999") == "ab-foot"


def test_reconcile_merged_pr_node_delegates_to_pr_number_reconcile(tmp_path, monkeypatch):
    # x-59a6: this call site no longer resolves a node or stamps pr_number
    # itself - it delegates entirely to `backlog reconcile --pr-number`,
    # which binds every node the PR's exact Backlog-Closure trailer names
    # (not just the one this process happens to find first) before running
    # the existing close scan. Repo scoping is explicit, resolved from the
    # PR's own url, never left to a cwd guess.
    url = f"{_FOOT}/777"
    g = _make_graph(tmp_path, [{"id": "ab-recon001", "title": "t", "pr_url": url}])
    _patch(monkeypatch, g)
    import fno.pr._merge as M
    monkeypatch.setattr(M, "_gh", _fake_gh_url(url))
    calls = []
    monkeypatch.setattr(M, "run", _stub_run(calls))
    M._reconcile_merged_pr_node(777, cwd=str(tmp_path))
    assert len(calls) == 1
    assert calls[0][-7:] == [
        "backlog", "reconcile", "--pr-number", "777", "--repo", "bllshttng/footnote", "--json",
    ]


def test_reconcile_backfills_pr_number_for_a_url_only_node(tmp_path, monkeypatch):
    # x-59a6 review fix: delegating to `backlog reconcile --pr-number` dropped
    # the pr_url-only backfill `_find_pr_node_id` used to provide. A node
    # stamped with pr_url but no pr_number (partial stamp, or an
    # off-convention branch name) must still get pr_number backfilled here,
    # since the delegated forward scan requires an int pr_number to find it.
    url = f"{_FOOT}/777"
    g = _make_graph(tmp_path, [{"id": "ab-recon001", "title": "t", "pr_url": url}])
    _patch(monkeypatch, g)
    import fno.pr._merge as M
    monkeypatch.setattr(M, "_gh", _fake_gh_url(url))
    monkeypatch.setattr(M, "run", _stub_run([]))
    M._reconcile_merged_pr_node(777, cwd=str(tmp_path))
    from fno.graph.store import read_graph
    node = next(e for e in read_graph(g) if e["id"] == "ab-recon001")
    assert node["pr_number"] == 777
    assert node["pr_url"] == url


def test_reconcile_does_not_clobber_existing_primary(tmp_path, monkeypatch):
    # The merged PR matches an additional_prs entry on a node that already has a
    # DIFFERENT primary; the primary number/url must be preserved (codex P2).
    url = f"{_FOOT}/777"
    g = _make_graph(tmp_path, [{
        "id": "ab-multi01", "title": "t",
        "pr_number": 100, "pr_url": f"{_FOOT}/100",
        "additional_prs": [{"number": 777, "url": url}],
    }])
    _patch(monkeypatch, g)
    import fno.pr._merge as M
    monkeypatch.setattr(M, "_gh", _fake_gh_url(url))
    calls = []
    monkeypatch.setattr(M, "run", _stub_run(calls))
    M._reconcile_merged_pr_node(777, cwd=str(tmp_path))
    from fno.graph.store import read_graph
    node = next(e for e in read_graph(g) if e["id"] == "ab-multi01")
    assert node["pr_number"] == 100                 # primary untouched
    assert node["pr_url"] == f"{_FOOT}/100"         # url pair intact
    assert len(calls) == 1                          # still closed, scoped to the node


def test_reconcile_surfaces_subprocess_failure(tmp_path, monkeypatch, capsys):
    # A non-zero reconcile must NOT be swallowed - that leaves the node open under a
    # green merge, the exact gap this closes (codex P2).
    url = f"{_FOOT}/321"
    g = _make_graph(tmp_path, [{"id": "ab-fail001", "title": "t", "pr_url": url}])
    _patch(monkeypatch, g)
    import fno.pr._merge as M
    monkeypatch.setattr(M, "_gh", _fake_gh_url(url))
    monkeypatch.setattr(M, "run", _stub_run([], ok=False, stderr="gh query unavailable"))
    M._reconcile_merged_pr_node(321, cwd=str(tmp_path))
    err = capsys.readouterr().err
    assert "reconcile for PR #321" in err and "failed" in err


def test_reconcile_merged_pr_node_delegates_even_without_a_locally_known_node(tmp_path, monkeypatch):
    # x-59a6: this call site no longer looks up a node before delegating - a
    # PR with no LOCALLY-matching node might still bind via its trailer
    # (a fresh node never seen before) or the reverse branch-name map, both
    # of which only `backlog reconcile --pr-number` itself can resolve.
    g = _make_graph(tmp_path, [{"id": "ab-other01", "title": "t",
                                "pr_number": 1, "pr_url": f"{_FOOT}/1"}])
    _patch(monkeypatch, g)
    import fno.pr._merge as M
    monkeypatch.setattr(M, "_gh", _fake_gh_url(f"{_FOOT}/999"))
    calls = []
    monkeypatch.setattr(M, "run", _stub_run(calls))
    M._reconcile_merged_pr_node(999, cwd=str(tmp_path))
    assert len(calls) == 1
    assert calls[0][-7:] == [
        "backlog", "reconcile", "--pr-number", "999", "--repo", "bllshttng/footnote", "--json",
    ]


def test_on_confirmed_merge_syncs_status_and_closes_node(tmp_path, monkeypatch):
    url = f"{_FOOT}/556"
    g = _make_graph(tmp_path, [{"id": "ab-conf001", "title": "t",
                                "pr_number": 556, "pr_url": url}])
    _patch(monkeypatch, g)
    _clear_env(monkeypatch)
    import fno.pr._merge as M
    monkeypatch.setattr(M, "_gh", _fake_gh_url(url))
    calls = []
    monkeypatch.setattr(M, "run", _stub_run(calls))
    M._on_confirmed_merge(556, str(tmp_path))
    from fno.graph.store import read_graph
    node = next(e for e in read_graph(g) if e["id"] == "ab-conf001")
    assert node.get("merge_status") == "merged"
    assert calls
    assert calls[0][-7:] == [
        "backlog", "reconcile", "--pr-number", "556", "--repo", "bllshttng/footnote", "--json",
    ]


def test_reconcile_merged_pr_node_closes_via_seam_under_external(
    tmp_path, monkeypatch
):
    """Under external selection the reconcile verb refuses, so the merge close
    must terminate through the shared seam: the primary-link backfill lands in
    the SIDECAR (the local graph is never written), exactly one tracker.close,
    and no reconcile subprocess fires."""
    from fno.tracker.types import NodeNotFound, TrackerCandidate, TrackerState

    url = f"{_FOOT}/777"
    g = _make_graph(tmp_path, [{"id": "ab-recon001", "title": "t", "pr_url": url}])
    _patch(monkeypatch, g)
    import fno.pr._merge as M

    monkeypatch.setattr(M, "_gh", _fake_gh_url(url))
    calls = []
    monkeypatch.setattr(M, "run", _stub_run(calls))

    class _T:
        name = "fake-external"

        def __init__(self):
            self.close_calls = []

        def read(self, id):
            if id != "ab-recon001":
                raise NodeNotFound(id)
            return TrackerCandidate(
                id=id, title="t", state=TrackerState.open,
                parent=None, blocked_by=[],
            )

        def list_open(self):
            return []

        def close(self, id):
            self.close_calls.append(id)

    tracker = _T()
    monkeypatch.setattr("fno.tracker.get_tracker", lambda *a, **k: tracker)
    sc_dir = tmp_path / "sidecars"
    sc_dir.mkdir()
    (sc_dir / "ab-recon001.json").write_text(json.dumps(
        {"id": "ab-recon001", "pr_url": url}))
    import fno.tracker.sidecar as sidecar_store

    monkeypatch.setattr(sidecar_store, "sidecar_path",
                        lambda i: sc_dir / f"{i}.json")
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    import fno.graph._constants as gc

    monkeypatch.setattr(gc, "LEDGER_JSON", tmp_path / "absent-ledger.json")

    from fno.graph._reconcile import PrMergeState

    monkeypatch.setattr(
        "fno.graph.cli._done_gh_query",
        lambda pr, **kw: PrMergeState(
            number=777, state="MERGED", url=url,
            merged_at="2026-08-17T00:00:00Z",
        ),
    )

    M._reconcile_merged_pr_node(777, cwd=str(tmp_path))
    assert tracker.close_calls == ["ab-recon001"]
    assert calls == []  # the refused reconcile subprocess never fired
    # The backfill went to the sidecar, not the graph.
    from fno.graph.store import read_graph

    node = next(e for e in read_graph(g) if e["id"] == "ab-recon001")
    assert node.get("pr_number") != 777
    sc = json.loads((sc_dir / "ab-recon001.json").read_text())
    assert sc["pr_number"] == 777


def test_reconcile_merged_pr_node_closes_via_seam_with_no_local_graph_file(
    tmp_path, monkeypatch
):
    """A project that has never used the default graph backend has no local
    graph.json at all. The stale "no graph, nothing to do" guard must not
    fire before the external branch is reached - it targets the graph-mode
    close path only."""
    from fno.tracker.types import NodeNotFound, TrackerCandidate, TrackerState

    url = f"{_FOOT}/777"
    absent_graph = tmp_path / "no-such-graph.json"
    assert not absent_graph.exists()
    import fno.pr._merge as M

    monkeypatch.setattr(M, "_gh", _fake_gh_url(url))
    calls = []
    monkeypatch.setattr(M, "run", _stub_run(calls))

    class _T:
        name = "fake-external"

        def __init__(self):
            self.close_calls = []

        def read(self, id):
            if id != "ab-recon001":
                raise NodeNotFound(id)
            return TrackerCandidate(
                id=id, title="t", state=TrackerState.open,
                parent=None, blocked_by=[],
            )

        def list_open(self):
            return []

        def close(self, id):
            self.close_calls.append(id)

    tracker = _T()
    monkeypatch.setattr("fno.tracker.get_tracker", lambda *a, **k: tracker)
    sc_dir = tmp_path / "sidecars"
    sc_dir.mkdir()
    (sc_dir / "ab-recon001.json").write_text(json.dumps(
        {"id": "ab-recon001", "pr_url": url}))
    import fno.tracker.sidecar as sidecar_store

    monkeypatch.setattr(sidecar_store, "sidecar_path",
                        lambda i: sc_dir / f"{i}.json")
    monkeypatch.setattr("fno.paths.graph_json", lambda: absent_graph)
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    import fno.graph._constants as gc

    monkeypatch.setattr(gc, "LEDGER_JSON", tmp_path / "absent-ledger.json")

    from fno.graph._reconcile import PrMergeState

    monkeypatch.setattr(
        "fno.graph.cli._done_gh_query",
        lambda pr, **kw: PrMergeState(
            number=777, state="MERGED", url=url,
            merged_at="2026-08-17T00:00:00Z",
        ),
    )

    M._reconcile_merged_pr_node(777, cwd=str(tmp_path))
    assert tracker.close_calls == ["ab-recon001"]
    assert calls == []


def test_on_confirmed_merge_leaves_graph_untouched_under_external(
    tmp_path, monkeypatch
):
    """merge_status is footnote-owned derived metadata; under external
    selection the whole confirmed-merge graph side-effect set must skip the
    local store (the close itself routes through the seam)."""
    from fno.tracker.types import NodeNotFound, TrackerCandidate, TrackerState

    url = f"{_FOOT}/888"
    g = _make_graph(tmp_path, [{"id": "ab-conf002", "title": "t",
                                "pr_number": 888, "pr_url": url}])
    _patch(monkeypatch, g)
    import fno.pr._merge as M

    monkeypatch.setattr(M, "_gh", _fake_gh_url(url))
    monkeypatch.setattr(M, "run", _stub_run([]))

    class _T:
        name = "fake-external"

        def read(self, id):
            if id != "ab-conf002":
                raise NodeNotFound(id)
            return TrackerCandidate(
                id=id, title="t", state=TrackerState.open,
                parent=None, blocked_by=[],
            )

        def list_open(self):
            return []

        def close(self, id):
            pass

    monkeypatch.setattr("fno.tracker.get_tracker", lambda *a, **k: _T())
    sc_dir = tmp_path / "sidecars"
    sc_dir.mkdir()
    (sc_dir / "ab-conf002.json").write_text(json.dumps(
        {"id": "ab-conf002", "pr_number": 888, "pr_url": url}))
    import fno.tracker.sidecar as sidecar_store

    monkeypatch.setattr(sidecar_store, "sidecar_path",
                        lambda i: sc_dir / f"{i}.json")
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    import fno.graph._constants as gc

    monkeypatch.setattr(gc, "LEDGER_JSON", tmp_path / "absent-ledger.json")

    from fno.graph._reconcile import PrMergeState

    monkeypatch.setattr(
        "fno.graph.cli._done_gh_query",
        lambda pr, **kw: PrMergeState(
            number=888, state="MERGED", url=url,
            merged_at="2026-08-17T00:00:00Z",
        ),
    )

    before = g.read_bytes()
    M._on_confirmed_merge(888, str(tmp_path))
    assert g.read_bytes() == before  # no graph write anywhere in the flow


# --- the merge mints its own cleanup request (the machine's reap order) ----------


def _write_manifest(tmp_path: Path) -> Path:
    state_dir = tmp_path / ".fno"
    state_dir.mkdir(exist_ok=True)
    manifest = state_dir / "target-state.md"
    manifest.write_text("---\nsession_id: sess-abc\nharness: claude\n---\n")
    return manifest


def _patch_events_log(monkeypatch, tmp_path: Path) -> Path:
    import fno.agents.events as E

    log = tmp_path / "agents-events.jsonl"
    monkeypatch.setattr(E, "daemon_lifecycle_log", lambda: log)
    return log


def _stub_gh_merged(monkeypatch, module, branch: str = "feature/x-07dc",
                    url: str = ""):
    class _R:
        ok = True
        stderr = ""
        stdout = ""

    def _gh(args, cwd):
        r = _R()
        fields = next((a for a in args if a.startswith("state,headRefName")), "")
        if "--json" in args and fields:
            r.stdout = json.dumps(
                {"state": "MERGED", "headRefName": branch, "url": url})
        return r

    monkeypatch.setattr(module, "_gh", _gh)


def _stub_git_root(monkeypatch, module, root: Path):
    class _R:
        ok = True
        stderr = ""
        stdout = str(root)

    monkeypatch.setattr(module, "_git", lambda args, cwd: _R())


def _requested_rows(log: Path) -> list[dict]:
    return [
        json.loads(line)
        for line in log.read_text().splitlines()
        if json.loads(line).get("type") == "merge_cleanup_requested"
    ]


def test_post_merge_followups_mints_cleanup_request(tmp_path, monkeypatch):
    # AC1-HP: a confirmed merge with no ritual run writes the request itself,
    # carrying merged_at, the closed node ids, and the session identity.
    import fno.agents.events as E
    import fno.pr._merge as M
    import fno.worktree_reapable as WR

    log = _patch_events_log(monkeypatch, tmp_path)
    _stub_gh_merged(monkeypatch, M)
    _stub_git_root(monkeypatch, M, tmp_path)
    M._REPO_ROOT_CACHE[str(tmp_path)] = str(tmp_path)
    _write_manifest(tmp_path)
    monkeypatch.setattr(WR, "is_linked_worktree", lambda p: True)
    monkeypatch.setattr(
        E, "rows_for_cleanup",
        lambda worktree, node_ids, runner=None: ["target-x-07dc-a1"],
    )

    M._run_post_merge_followups(9, "squash", str(tmp_path), bound_node_ids=["x-07dc"])

    rows = _requested_rows(log)
    assert len(rows) == 1
    data = rows[0]["data"]
    assert data["pr"] == 9
    assert data["branch"] == "feature/x-07dc"
    assert data["node_ids"] == ["x-07dc"]
    assert data["session_id"] == "sess-abc"
    assert data["harness"] == "claude"
    assert data["candidate_row_names"] == ["target-x-07dc-a1"]
    assert data["merged_at"] and data["merged_at"].endswith("Z")
    assert data["request_id"].startswith("merge-cleanup-")


def test_merge_with_no_bound_nodes_still_mints_empty(tmp_path, monkeypatch):
    # Held shape: a reconcile that bound nothing still mints, with
    # node_ids [] - the daemon's doneness re-read holds that request.
    import fno.pr._merge as M
    import fno.worktree_reapable as WR

    log = _patch_events_log(monkeypatch, tmp_path)
    _stub_gh_merged(monkeypatch, M)
    _stub_git_root(monkeypatch, M, tmp_path)
    M._REPO_ROOT_CACHE[str(tmp_path)] = str(tmp_path)
    _write_manifest(tmp_path)
    monkeypatch.setattr(WR, "is_linked_worktree", lambda p: False)

    M._run_post_merge_followups(9, "squash", str(tmp_path), bound_node_ids=[])

    rows = _requested_rows(log)
    assert len(rows) == 1
    assert rows[0]["data"]["node_ids"] == []
    assert rows[0]["data"]["worktree"] is None


def test_ritual_mint_shares_request_id_with_merge_mint(tmp_path, monkeypatch):
    # AC1-EDGE: the ritual's mint and the merge's mint carry ONE request id,
    # so the daemon's fold keeps a single pending request, not two.
    import fno.agents.events as E
    import fno.pr._ritual as R

    log = _patch_events_log(monkeypatch, tmp_path)
    monkeypatch.setattr(
        R, "rows_for_cleanup",
        lambda worktree, node_ids, runner=None: ["target-x-07dc-a1"],
    )
    monkeypatch.setattr(R, "agents_home_dir", lambda: tmp_path / "agents-home")
    ritual = R.Ritual.__new__(R.Ritual)
    ritual.cwd = tmp_path
    # The mint reads the memoized gh read for the grace anchor; a bare
    # __new__ ritual has no runner, so seed the cache the legs would have.
    ritual._merge_state = ("MERGED", "feature/x-07dc", "2026-09-07T15:00:00Z")
    ritual.ctx = R._Ctx(
        pr=9,
        autonomous=False,
        canon=tmp_path,
        settings=None,
        pm=None,
        project="proj",
        lane_project="",
        parking_lot=None,
        holder="",
        node_ids=["x-07dc"],
    )
    order = ritual._register_cleanup_request(
        "feature/x-07dc", str(tmp_path / "wt"), "merged-pr"
    )
    twin = E.emit_merge_cleanup_requested(
        repo=str(tmp_path),
        project="proj",
        pr=9,
        branch="feature/x-07dc",
        worktree=str(tmp_path / "wt"),
        node_ids=["x-07dc"],
        session_id=None,
        harness=None,
        candidate_row_names=[],
    )
    ids = [row["data"]["request_id"] for row in _requested_rows(log)]
    assert "cleanup-requested" in order
    assert ids == [twin, twin]


def _patch_sidecar(monkeypatch, rows):
    from fno.tracker import sidecar as sidecar_store
    from fno.tracker.sidecar import Sidecar

    monkeypatch.setattr(
        sidecar_store,
        "load_all",
        lambda: {
            e["id"]: Sidecar(id=e["id"], pr_number=e.get("pr_number"),
                             pr_url=e.get("pr_url"))
            for e in rows
        },
    )


def test_merge_mint_recovers_node_ids_from_sidecar(tmp_path, monkeypatch):
    # AC1-HP: a merge whose reconcile bound nothing recovers the PR's node
    # ids from the sidecar store (repo-scoped off the PR url), so the request
    # names what it can reap instead of holding on no-node-ids for a day.
    import fno.agents.events as E
    import fno.pr._merge as M
    import fno.worktree_reapable as WR

    log = _patch_events_log(monkeypatch, tmp_path)
    _stub_gh_merged(monkeypatch, M, url="https://github.com/owner/repo/pull/7")
    _stub_git_root(monkeypatch, M, tmp_path)
    M._REPO_ROOT_CACHE[str(tmp_path)] = str(tmp_path)
    _write_manifest(tmp_path)
    monkeypatch.setattr(WR, "is_linked_worktree", lambda p: False)
    _patch_sidecar(monkeypatch, [
        {"id": "fno-abc1", "pr_number": 7,
         "pr_url": "https://github.com/owner/repo/pull/7"}])

    M._run_post_merge_followups(7, "squash", str(tmp_path), bound_node_ids=[])

    rows = _requested_rows(log)
    assert len(rows) == 1
    assert rows[0]["data"]["node_ids"] == ["fno-abc1"]


def test_both_mints_carry_one_request_id(tmp_path, monkeypatch):
    # AC1-FOLD: the merge mint (worktree /wt, node_ids []) and the ritual
    # mint (worktree null, node_ids [x-1]) key on project, PR and branch,
    # so one merge produces ONE request id either way.
    import fno.agents.events as E

    log = _patch_events_log(monkeypatch, tmp_path)
    _patch_sidecar(monkeypatch, [])
    E.emit_merge_cleanup_requested(
        repo=str(tmp_path), project="proj", pr=7, branch="feature/x",
        worktree="/wt", node_ids=[], session_id=None, harness=None,
        candidate_row_names=[], repo_slug="owner/repo",
    )
    E.emit_merge_cleanup_requested(
        repo=str(tmp_path), project="proj", pr=7, branch="feature/x",
        worktree=None, node_ids=["x-1"], session_id=None, harness=None,
        candidate_row_names=[],
    )
    ids = [row["data"]["request_id"] for row in _requested_rows(log)]
    assert ids[0] == ids[1]


def test_merge_mint_excludes_foreign_repo_sidecar_nodes(tmp_path, monkeypatch):
    # AC1-EDGE: a sidecar node whose pr_url names another repo sharing the
    # PR number stays out of the recovered ids (repo-scoped, never guessed).
    import fno.pr._merge as M
    import fno.worktree_reapable as WR

    log = _patch_events_log(monkeypatch, tmp_path)
    _stub_gh_merged(monkeypatch, M, url="https://github.com/owner/repo/pull/7")
    _stub_git_root(monkeypatch, M, tmp_path)
    M._REPO_ROOT_CACHE[str(tmp_path)] = str(tmp_path)
    _write_manifest(tmp_path)
    monkeypatch.setattr(WR, "is_linked_worktree", lambda p: False)
    _patch_sidecar(monkeypatch, [
        {"id": "fno-forei", "pr_number": 7,
         "pr_url": "https://github.com/other/repo/pull/7"}])

    M._run_post_merge_followups(7, "squash", str(tmp_path), bound_node_ids=[])

    rows = _requested_rows(log)
    assert len(rows) == 1
    assert rows[0]["data"]["node_ids"] == []
