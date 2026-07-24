"""Tests for `fno backlog reconcile` (close merged-PR drift).

Covers AC1-AC5 from internal/fno/plans/2026-05-24-backlog-reconcile.md:
  AC1 happy path  - open node + MERGED PR -> done; second run is a no-op
  AC2 no clobber  - already-done node left untouched
  AC3 PR open     - open node whose PR is OPEN -> untouched
  AC4 dry-run     - graph.json byte-identical before/after
  AC5 judgment    - retro sentinel dropped, no new graph/inbox entries

Split into pure-module tests (stubbed query, no I/O) and CLI tests
(CliRunner + a tmp graph + monkeypatched gh query). The gh PR query is
never hit - tests stub it.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.graph import _reconcile as rec
from fno.graph._reconcile import (
    MergeDriftRecord,
    PrMergeState,
    ReconcileError,
    node_is_open,
    node_pr_refs,
    scan_merge_drift,
    write_retro_sentinel,
)

runner = CliRunner()


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _patch_graph_path(monkeypatch, graph_path: Path) -> None:
    """Point all graph-path constants at a tmp file (mirrors test_graph_hygiene)."""
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", graph_path)
    monkeypatch.setattr(gc, "GRAPH_MD", graph_path.parent / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", graph_path.parent / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", graph_path)


def _make_graph(path: Path, entries: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


@pytest.fixture(autouse=True)
def _no_revert_fetch(monkeypatch):
    """Keep reconcile hermetic: never shell `gh pr list` from tests. W4 revert
    detection has its own unit tests (test_causal_fields.py); the reverse-map
    pass has its own stubbed tests below."""
    monkeypatch.setattr(rec, "fetch_recent_merged_prs", lambda **kw: [])
    monkeypatch.setattr(rec, "list_merged_pr_branches", lambda **kw: [])


def _stub_query(state_by_number: dict[int, str]):
    """Return a query stub mapping pr_number -> state string."""

    def _q(number: int, repo=None, cwd=None) -> PrMergeState:
        st = state_by_number.get(number, "OPEN")
        return PrMergeState(
            number=number,
            state=st,
            url=f"https://github.com/o/r/pull/{number}",
            merged_at="2026-05-24T00:00:00Z" if st == "MERGED" else None,
        )

    return _q


def _read_entries(path: Path) -> list[dict]:
    return json.loads(path.read_text())["entries"]


def _node(node_id: str, **over) -> dict:
    base = {
        "id": node_id,
        "title": node_id,
        "pr_number": None,
        "pr_url": None,
        "additional_prs": [],
        "completed_at": None,
        "superseded_by": None,
        "plan_path": None,
        "cwd": None,
    }
    base.update(over)
    # Give a parseable GitHub PR URL by default so scan can derive the repo
    # (reconcile refuses to query without repo context). Tests that exercise
    # the no-repo-context path pass pr_url explicitly (or leave it None).
    if base.get("pr_number") and "pr_url" not in over:
        base["pr_url"] = f"https://github.com/test-owner/test-repo/pull/{base['pr_number']}"
    return base


# ---------------------------------------------------------------------------
# Pure-module tests
# ---------------------------------------------------------------------------

def test_node_is_open():
    assert node_is_open(_node("ab-1"))
    assert not node_is_open(_node("ab-2", completed_at="2026-01-01T00:00:00Z"))
    assert not node_is_open(_node("ab-3", superseded_by="ab-9"))


def test_node_pr_refs_primary_and_additional_dedup():
    node = _node(
        "ab-1",
        pr_number=10,
        pr_url="u10",
        additional_prs=[{"number": 11, "url": "u11"}, {"number": 10, "url": "dup"}],
    )
    refs = node_pr_refs(node)
    assert refs == [(10, "u10"), (11, "u11")]  # primary wins, dup dropped


def test_scan_finds_merged_pr():
    entries = [_node("ab-merged", pr_number=42)]
    records = scan_merge_drift(entries, query=_stub_query({42: "MERGED"}))
    assert len(records) == 1
    assert records[0].closeable
    assert records[0].pr_number == 42


def test_scan_skips_open_pr():
    entries = [_node("ab-open", pr_number=7)]
    records = scan_merge_drift(entries, query=_stub_query({7: "OPEN"}))
    assert records == []  # AC3: not drift


def test_scan_skips_done_node():
    entries = [_node("ab-done", pr_number=5, completed_at="2026-01-01T00:00:00Z")]
    records = scan_merge_drift(entries, query=_stub_query({5: "MERGED"}))
    assert records == []  # closed nodes are never scanned


def test_scan_node_filter():
    entries = [_node("ab-a", pr_number=1), _node("ab-b", pr_number=2)]
    q = _stub_query({1: "MERGED", 2: "MERGED"})
    records = scan_merge_drift(entries, query=q, node_id="ab-b")
    assert [r.node_id for r in records] == ["ab-b"]


def test_scan_query_error_surfaces_failure():
    def _boom(number, repo=None, cwd=None):
        raise ReconcileError("gh exploded")

    entries = [_node("ab-x", pr_number=99)]
    records = scan_merge_drift(entries, query=_boom)
    assert len(records) == 1
    assert not records[0].closeable
    assert records[0].error == "gh exploded"


def test_repo_slug_from_url():
    from fno.graph._reconcile import repo_slug_from_url

    assert repo_slug_from_url("https://github.com/bllshttng/footnote/pull/342") == "bllshttng/footnote"
    assert repo_slug_from_url("https://github.com/o/r/issues/9") == "o/r"
    assert repo_slug_from_url("not a url") is None
    assert repo_slug_from_url(None) is None


def test_scan_no_repo_context_is_failure():
    """A node with a PR number but no parseable URL and no cwd is a failure,
    never a close - guards against resolving against the wrong repo."""
    def _q(number, repo=None, cwd=None) -> PrMergeState:
        return PrMergeState(number=number, state="MERGED", url=None,
                            merged_at="2026-05-24T00:00:00Z")

    entries = [_node("ab-norepo", pr_number=55, pr_url=None, cwd=None)]
    records = scan_merge_drift(entries, query=_q)
    assert len(records) == 1
    assert not records[0].closeable
    assert "no repo context" in records[0].error


def test_scan_passes_repo_from_url():
    """The repo derived from the PR URL is passed to the query."""
    seen = {}

    def _q(number, repo=None, cwd=None) -> PrMergeState:
        seen["repo"] = repo
        return PrMergeState(number=number, state="MERGED", url=None,
                            merged_at="2026-05-24T00:00:00Z")

    entries = [_node("ab-r", pr_number=77)]  # default url -> test-owner/test-repo
    scan_merge_drift(entries, query=_q)
    assert seen["repo"] == "test-owner/test-repo"


# ---------------------------------------------------------------------------
# Reverse-map: unstamped open nodes matched by merged branch name (x-8c3b)
# ---------------------------------------------------------------------------

@pytest.fixture
def live_cwd(tmp_path) -> str:
    """An existing dir standing in for a live worktree cwd. The reverse-map
    dead-cwd guard (x-4114) skips any node whose cwd is not an existing dir, so
    tests exercising the gh-query path must anchor on a real directory."""
    return str(tmp_path)


def _merged(node_id_to_branch: dict, url_owner: str = "test-owner/test-repo"):
    """Return a list_merged stub: yields merged-PR rows for the given branches.

    ``node_id_to_branch`` maps a synthetic PR number -> headRefName.
    """
    rows = [
        {
            "number": num,
            "url": f"https://github.com/{url_owner}/pull/{num}",
            "headRefName": branch,
            "mergedAt": "2026-07-08T00:00:00Z",
        }
        for num, branch in node_id_to_branch.items()
    ]
    return lambda **kw: rows


def test_reverse_map_legacy_feature_branch(live_cwd):
    """Unstamped open node + merged `feature/<id>` -> closeable drift record."""
    entries = [_node("ab-rev1", cwd=live_cwd)]  # no pr_number -> ref-less
    records = scan_merge_drift(entries, list_merged=_merged({268: "feature/ab-rev1"}))
    assert len(records) == 1
    r = records[0]
    assert r.node_id == "ab-rev1" and r.closeable
    assert r.pr_number == 268
    assert r.pr_url.endswith("/pull/268")


def test_reverse_map_slug_branch(live_cwd):
    """Unstamped node + merged `<prefix>/<slug>-<id>` -> matches."""
    entries = [_node("ab-rev2", cwd=live_cwd)]
    records = scan_merge_drift(
        entries, list_merged=_merged({270: "target/some-slug-ab-rev2"})
    )
    assert len(records) == 1 and records[0].closeable
    assert records[0].pr_number == 270


def test_reverse_map_prefix_collision_guard(live_cwd):
    """id `ab-rev` must NOT match branch `feature/ab-rev7` or `feature/ab-reva`."""
    entries = [_node("ab-rev", cwd=live_cwd)]
    records = scan_merge_drift(
        entries,
        list_merged=_merged({1: "feature/ab-rev7", 2: "feature/ab-reva"}),
    )
    assert records == []


def test_reverse_map_no_match_leaves_open(live_cwd):
    """No merged branch carries the id -> no record, node stays open."""
    entries = [_node("ab-rev3", cwd=live_cwd)]
    records = scan_merge_drift(entries, list_merged=_merged({9: "feature/other-node"}))
    assert records == []


def test_reverse_map_ambiguous_is_error(live_cwd):
    """Two merged PRs match one id -> error record, never an auto-close."""
    entries = [_node("ab-dup", cwd=live_cwd)]
    records = scan_merge_drift(
        entries,
        list_merged=_merged({10: "feature/ab-dup", 11: "target/x-ab-dup"}),
    )
    assert len(records) == 1
    assert not records[0].closeable
    assert "#10" in records[0].error and "#11" in records[0].error


def test_reverse_map_skips_stamped_nodes():
    """A node WITH a pr ref never enters the reverse pass (forward path owns it).

    The list_merged stub raises if called for such a node, proving it is never
    consulted for stamped nodes.
    """
    def _boom(**kw):
        raise AssertionError("list_merged must not run for stamped nodes")

    entries = [_node("ab-stamped", pr_number=42, cwd="/repo")]
    records = scan_merge_drift(
        entries, query=_stub_query({42: "OPEN"}), list_merged=_boom
    )
    assert records == []  # OPEN forward state, no reverse pass


def test_reverse_map_requires_cwd():
    """A ref-less node with no cwd is skipped (no repo to query)."""
    def _boom(**kw):
        raise AssertionError("list_merged must not run without a cwd")

    entries = [_node("ab-nocwd", cwd=None)]
    records = scan_merge_drift(entries, list_merged=_boom)
    assert records == []


def test_reverse_map_gh_failure_surfaces_error(live_cwd):
    """A gh failure during reverse-map yields an error record, not a silent drop.

    AC2-ERR: the node's cwd EXISTS (live worktree) but gh exits non-zero - the
    real-failure path, distinct from the dead-cwd skip.
    """
    def _boom(**kw):
        raise rec.ReconcileError("gh list exploded")

    entries = [_node("ab-fail", cwd=live_cwd)]
    records = scan_merge_drift(entries, list_merged=_boom)
    assert len(records) == 1
    assert not records[0].closeable
    assert "gh list exploded" in records[0].error


def test_reverse_map_one_gh_call_per_repo(live_cwd):
    """Multiple ref-less nodes sharing a cwd trigger ONE gh call, not N."""
    calls = {"n": 0}

    def _once(**kw):
        calls["n"] += 1
        return [
            {"number": 1, "url": "u1", "headRefName": "feature/ab-m1",
             "mergedAt": "2026-07-08T00:00:00Z"},
            {"number": 2, "url": "u2", "headRefName": "feature/ab-m2",
             "mergedAt": "2026-07-08T00:00:00Z"},
        ]

    entries = [_node("ab-m1", cwd=live_cwd), _node("ab-m2", cwd=live_cwd)]
    records = scan_merge_drift(entries, list_merged=_once)
    assert calls["n"] == 1
    assert {r.node_id for r in records} == {"ab-m1", "ab-m2"}


# ---------------------------------------------------------------------------
# Reverse-map: deleted-worktree cwd falls back to the project root (x-3dd0)
# ---------------------------------------------------------------------------

def test_reverse_map_gone_cwd_falls_back_to_project_root(tmp_path, monkeypatch):
    """AC1-HP: a gone recorded cwd reverse-maps from the node's project root."""
    import fno.graph._intake as intake

    root = tmp_path / "proj-root"
    root.mkdir()
    monkeypatch.setattr(
        intake, "project_root_from_settings",
        lambda project: str(root) if project == "myproj" else None,
    )

    seen: dict = {}

    def _spy(**kw):
        seen["cwd"] = kw.get("cwd")
        return [{"number": 5, "url": "u5", "headRefName": "feature/ab-gone",
                 "mergedAt": "2026-07-08T00:00:00Z"}]

    gone = str(tmp_path / "archived-worktree")  # never created -> gone
    entries = [_node("ab-gone", cwd=gone, project="myproj")]
    records = scan_merge_drift(entries, list_merged=_spy)
    assert seen["cwd"] == str(root)  # resolved root, not the gone cwd
    assert len(records) == 1 and records[0].closeable
    assert records[0].pr_number == 5


@pytest.mark.parametrize("resolved", [None, "__nonexistent__"])
def test_reverse_map_gone_cwd_unresolvable_is_skipped(tmp_path, monkeypatch, capsys, resolved):
    """AC1-HP / US1: gone cwd + (unmapped project | mapped-but-missing root) ->
    node SKIPPED, gh never called, one aggregated advisory instead of Errno 2."""
    import fno.graph._intake as intake

    missing_root = str(tmp_path / "also-gone")
    monkeypatch.setattr(
        intake, "project_root_from_settings",
        lambda project: (missing_root if resolved == "__nonexistent__" else None),
    )

    gone = str(tmp_path / "gone-wt")

    def _boom(**kw):
        raise AssertionError("list_merged must not run for a dead-cwd node")

    entries = [_node("ab-unresolvable", cwd=gone, project="p")]
    records = scan_merge_drift(entries, list_merged=_boom)
    assert records == []  # skipped: no record (neither closeable nor error)
    err = capsys.readouterr().err
    assert "reverse-map: skipped 1 ref-less node(s)" in err
    assert "ab-unresolvable" in err
    assert "fno backlog update" in err


def test_reverse_map_advisory_aggregates_and_is_silent_when_clean(tmp_path, monkeypatch, capsys):
    """US2: N dead-cwd nodes -> exactly one advisory line naming all ids; a graph
    with no dead-cwd node prints nothing."""
    import fno.graph._intake as intake
    monkeypatch.setattr(intake, "project_root_from_settings", lambda project: None)

    gone = str(tmp_path / "nope")
    entries = [_node(nid, cwd=gone) for nid in ("ab-d1", "ab-d2", "ab-d3")]
    scan_merge_drift(entries, list_merged=lambda **kw: [])
    err = capsys.readouterr().err.strip()
    assert err.count("reverse-map: skipped") == 1
    assert "skipped 3 ref-less node(s)" in err
    for nid in ("ab-d1", "ab-d2", "ab-d3"):
        assert nid in err

    # Clean graph (live cwd, no match): no advisory at all.
    entries = [_node("ab-clean", cwd=str(tmp_path))]
    scan_merge_drift(entries, list_merged=lambda **kw: [])
    assert "reverse-map: skipped" not in capsys.readouterr().err


def test_reverse_map_advisory_names_every_id_no_truncation(tmp_path, monkeypatch, capsys):
    """Visibility invariant: a large batch names EVERY skipped id on one line -
    no cap, no `(+N more)` collapse (a truncated tail would be un-healable)."""
    import fno.graph._intake as intake
    monkeypatch.setattr(intake, "project_root_from_settings", lambda project: None)

    gone = str(tmp_path / "gone")
    ids = [f"ab-dead{i:02d}" for i in range(12)]  # > the old 10-id cap
    scan_merge_drift([_node(nid, cwd=gone) for nid in ids],
                     list_merged=lambda **kw: [])
    err = capsys.readouterr().err
    assert "skipped 12 ref-less node(s)" in err
    assert "more)" not in err  # no truncation suffix
    for nid in ids:
        assert nid in err


def test_forward_dead_cwd_with_pr_url_queries_with_repo(tmp_path, monkeypatch):
    """AC4-EDGE / US3: a stamped node whose cwd is gone but pr_url is parseable
    resolves via --repo with cwd=None and closes on MERGED - no Errno 2."""
    import fno.graph._intake as intake
    monkeypatch.setattr(intake, "project_root_from_settings", lambda project: None)

    seen: dict = {}

    def _q(number, repo=None, cwd=None) -> PrMergeState:
        seen["repo"], seen["cwd"] = repo, cwd
        return PrMergeState(number=number, state="MERGED", url=None,
                            merged_at="2026-05-24T00:00:00Z")

    gone = str(tmp_path / "archived-wt")
    entries = [_node("ab-fwd", pr_number=88, cwd=gone)]  # default url -> parseable repo
    records = scan_merge_drift(entries, query=_q)
    assert seen["repo"] == "test-owner/test-repo"
    assert seen["cwd"] is None  # dead cwd degraded, never handed to subprocess
    assert len(records) == 1 and records[0].closeable


def test_forward_dead_cwd_no_repo_context_is_error(tmp_path, monkeypatch):
    """AC5-EDGE / US3: a stamped node with an unparseable pr_url and a dead cwd
    yields the existing `no repo context` error, not an Errno 2."""
    import fno.graph._intake as intake
    monkeypatch.setattr(intake, "project_root_from_settings", lambda project: None)

    def _q(number, repo=None, cwd=None) -> PrMergeState:
        raise AssertionError("query must not run without repo context")

    gone = str(tmp_path / "archived-wt")
    entries = [_node("ab-norepo-fwd", pr_number=91, pr_url=None, cwd=gone)]
    records = scan_merge_drift(entries, query=_q)
    assert len(records) == 1
    assert not records[0].closeable
    assert "no repo context" in records[0].error


def test_reverse_map_existing_cwd_skips_resolver(tmp_path, monkeypatch):
    """AC3-EDGE: a live recorded cwd is used as-is; the resolver never runs."""
    import fno.graph._intake as intake

    def _no_resolve(project):
        raise AssertionError("project_root_from_settings must not run for a live cwd")

    monkeypatch.setattr(intake, "project_root_from_settings", _no_resolve)

    seen: dict = {}

    def _spy(**kw):
        seen["cwd"] = kw.get("cwd")
        return [{"number": 7, "url": "u7", "headRefName": "feature/ab-live",
                 "mergedAt": "2026-07-08T00:00:00Z"}]

    live = str(tmp_path)  # exists on disk
    entries = [_node("ab-live", cwd=live, project="whatever")]
    records = scan_merge_drift(entries, list_merged=_spy)
    assert seen["cwd"] == live
    assert len(records) == 1 and records[0].pr_number == 7


def test_reverse_map_gone_cwd_same_project_one_call(tmp_path, monkeypatch):
    """AC4-EDGE: two gone-cwd nodes of one project collapse to ONE gh call."""
    import fno.graph._intake as intake

    root = tmp_path / "shared-root"
    root.mkdir()
    monkeypatch.setattr(intake, "project_root_from_settings", lambda project: str(root))

    calls = {"n": 0, "cwds": []}

    def _once(**kw):
        calls["n"] += 1
        calls["cwds"].append(kw.get("cwd"))
        return [
            {"number": 1, "url": "u1", "headRefName": "feature/ab-c1",
             "mergedAt": "2026-07-08T00:00:00Z"},
            {"number": 2, "url": "u2", "headRefName": "feature/ab-c2",
             "mergedAt": "2026-07-08T00:00:00Z"},
        ]

    entries = [
        _node("ab-c1", cwd=str(tmp_path / "wt1"), project="p"),
        _node("ab-c2", cwd=str(tmp_path / "wt2"), project="p"),
    ]
    records = scan_merge_drift(entries, list_merged=_once)
    assert calls["n"] == 1
    assert calls["cwds"] == [str(root)]
    assert {r.node_id for r in records} == {"ab-c1", "ab-c2"}


def test_effective_reconcile_cwd(tmp_path, monkeypatch):
    """The deleted-worktree cwd fallback shared by the reverse-map query AND the
    post-close auto-continue routing (x-3dd0)."""
    import fno.graph._intake as intake

    real_root = tmp_path / "proj"
    real_root.mkdir()
    monkeypatch.setattr(
        intake, "project_root_from_settings",
        lambda project: str(real_root) if project == "p" else None,
    )

    live = str(tmp_path)  # exists on disk
    gone = str(tmp_path / "gone-wt")

    assert rec._effective_reconcile_cwd(live, "p") == live          # live -> untouched
    assert rec._effective_reconcile_cwd(gone, "p") == str(real_root)  # gone -> project root
    assert rec._effective_reconcile_cwd(gone, "other") == gone      # unmapped -> original

    # mapped-but-missing root -> original cwd kept (unchanged degrade)
    monkeypatch.setattr(
        intake, "project_root_from_settings", lambda project: str(tmp_path / "also-gone")
    )
    assert rec._effective_reconcile_cwd(gone, "p") == gone


def test_write_retro_sentinel(tmp_path):
    record = MergeDriftRecord(
        node_id="ab-sent",
        plan_path="plans/x.md",
        pr_number=3,
        pr_url="u3",
        pr_state="MERGED",
        merged_at="2026-05-24T00:00:00Z",
    )
    path = write_retro_sentinel(record, sentinel_dir=tmp_path / "retro")
    assert path.exists()
    payload = json.loads(path.read_text())
    assert payload["node_id"] == "ab-sent"
    assert payload["pr_number"] == 3
    assert payload["closed_by"] == "backlog-reconcile"


# ---------------------------------------------------------------------------
# CLI tests
# ---------------------------------------------------------------------------

@pytest.fixture
def cli_env(tmp_path, monkeypatch):
    """Tmp graph + tmp retro sentinel dir + a no-op plan stamp."""
    graph_path = tmp_path / "graph.json"
    _patch_graph_path(monkeypatch, graph_path)

    sentinel_dir = tmp_path / "retro-pending"
    import fno.paths as paths
    monkeypatch.setattr(paths, "retro_pending_dir", lambda: sentinel_dir)

    return graph_path, sentinel_dir


def test_reconcile_happy_path_then_noop(cli_env, monkeypatch):
    """AC1: open node + MERGED PR -> done; a second run mutates nothing."""
    graph_path, sentinel_dir = cli_env
    _make_graph(graph_path, [_node("ab-hp", pr_number=100)])
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({100: "MERGED"}))

    result = runner.invoke(app, ["backlog", "reconcile"])
    assert result.exit_code == 0, result.output

    entries = _read_entries(graph_path)
    node = next(e for e in entries if e["id"] == "ab-hp")
    assert node["completed_at"] is not None
    assert node["status"] == "done"
    assert (sentinel_dir / "ab-hp.json").exists()

    # Second run: node now done -> scan skips it -> completed_at unchanged.
    first_ts = node["completed_at"]
    result2 = runner.invoke(app, ["backlog", "reconcile"])
    assert result2.exit_code == 0, result2.output
    node2 = next(e for e in _read_entries(graph_path) if e["id"] == "ab-hp")
    assert node2["completed_at"] == first_ts


def test_reconcile_does_not_dispatch_post_merge_ritual(cli_env, monkeypatch):
    """AC10-EDGE: reconcile is no longer a ritual detector. Closing a merged node
    with post_merge.auto_run armed must NOT invoke the dispatch seam, run the
    verb, or write a dispatch marker - pr-watch is the sole detector now."""
    import fno.post_merge_route as pmr

    graph_path, _sentinel = cli_env
    repo_dir = graph_path.parent
    _make_graph(graph_path, [_node("ab-pm", pr_number=200, cwd=str(repo_dir))])
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({200: "MERGED"}))
    # Arm auto_run so the pre-cutover reconcile leg WOULD have dispatched; the
    # cutover means this now does nothing.
    (repo_dir / ".fno").mkdir(parents=True, exist_ok=True)
    (repo_dir / ".fno" / "config.toml").write_text("[post_merge]\nauto_run = true\n")

    dispatched: list = []
    monkeypatch.setattr(
        pmr, "dispatch_post_merge_ritual",
        lambda *a, **k: dispatched.append(a) or pmr.PostMergeDispatchResult("dispatched", 200),
    )

    result = runner.invoke(app, ["backlog", "reconcile"])
    assert result.exit_code == 0, result.output

    # Reconcile still does its day job (closes the node)...
    node = next(e for e in _read_entries(graph_path) if e["id"] == "ab-pm")
    assert node["completed_at"] is not None
    # ...but never dispatches a ritual and writes no marker.
    assert dispatched == []
    assert not (repo_dir / ".fno" / "post-merge-dispatched").exists()


def test_reconcile_backfills_reverse_mapped_pr_ref(cli_env, tmp_path, monkeypatch):
    """A reverse-mapped node (dead before stamp) gets its recovered PR ref
    written back to the graph, not just closed - else the board loses the PR
    link and detect_reverted_nodes cannot match a later revert (codex P2)."""
    graph_path, sentinel_dir = cli_env
    # Live cwd: the reverse-map dead-cwd guard (x-4114) skips a node whose cwd is
    # gone, so a close-path test must anchor on a real dir.
    _make_graph(graph_path, [_node("ab-rmap", cwd=str(tmp_path))])  # ref-less
    monkeypatch.setattr(
        rec, "list_merged_pr_branches",
        lambda **kw: [{
            "number": 268, "url": "https://github.com/o/r/pull/268",
            "headRefName": "feature/ab-rmap", "mergedAt": "2026-07-08T00:00:00Z",
        }],
    )

    result = runner.invoke(app, ["backlog", "reconcile"])
    assert result.exit_code == 0, result.output

    node = next(e for e in _read_entries(graph_path) if e["id"] == "ab-rmap")
    assert node["completed_at"] is not None
    assert node["pr_number"] == 268
    assert node["pr_url"] == "https://github.com/o/r/pull/268"


def test_reconcile_no_clobber(cli_env, monkeypatch):
    """AC2: an already-done node is left untouched (no timestamp churn)."""
    graph_path, _ = cli_env
    done_ts = "2026-01-01T00:00:00+00:00"
    _make_graph(graph_path, [_node("ab-done", pr_number=200, completed_at=done_ts)])
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({200: "MERGED"}))

    result = runner.invoke(app, ["backlog", "reconcile"])
    assert result.exit_code == 0, result.output

    node = next(e for e in _read_entries(graph_path) if e["id"] == "ab-done")
    assert node["completed_at"] == done_ts  # unchanged


def test_reconcile_pr_open_untouched(cli_env, monkeypatch):
    """AC3: open node whose PR is still OPEN is not closed."""
    graph_path, sentinel_dir = cli_env
    _make_graph(graph_path, [_node("ab-op", pr_number=300)])
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({300: "OPEN"}))

    result = runner.invoke(app, ["backlog", "reconcile"])
    assert result.exit_code == 0, result.output

    node = next(e for e in _read_entries(graph_path) if e["id"] == "ab-op")
    assert node["completed_at"] is None
    assert not (sentinel_dir / "ab-op.json").exists()


def test_reconcile_dry_run_byte_identical(cli_env, monkeypatch):
    """AC4: --dry-run reports candidates but leaves graph.json byte-identical."""
    graph_path, sentinel_dir = cli_env
    _make_graph(graph_path, [_node("ab-dry", pr_number=400)])
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({400: "MERGED"}))

    before = graph_path.read_bytes()
    result = runner.invoke(app, ["backlog", "reconcile", "--dry-run"])
    assert result.exit_code == 0, result.output
    after = graph_path.read_bytes()

    assert before == after  # nothing mutated
    assert "ab-dry" in result.output
    assert not (sentinel_dir / "ab-dry.json").exists()


def test_reconcile_judgment_not_automated(cli_env, monkeypatch):
    """AC5: sentinel dropped, no new graph entries, sentinel is the only artifact."""
    graph_path, sentinel_dir = cli_env
    _make_graph(graph_path, [_node("ab-j", pr_number=500)])
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({500: "MERGED"}))

    n_before = len(_read_entries(graph_path))
    result = runner.invoke(app, ["backlog", "reconcile"])
    assert result.exit_code == 0, result.output

    entries_after = _read_entries(graph_path)
    assert len(entries_after) == n_before  # no node auto-created
    sentinels = list(sentinel_dir.glob("*.json"))
    assert [p.name for p in sentinels] == ["ab-j.json"]  # only the one sentinel


def test_reconcile_json_output(cli_env, monkeypatch):
    """--json emits a parseable summary naming the closed node."""
    graph_path, _ = cli_env
    _make_graph(graph_path, [_node("ab-json", pr_number=600)])
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({600: "MERGED"}))

    result = runner.invoke(app, ["backlog", "reconcile", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["dry_run"] is False
    assert [c["node_id"] for c in payload["closed"]] == ["ab-json"]


def test_reconcile_triggers_advance_after_close(cli_env, monkeypatch):
    """Task 2.1: after closing a drifted node, reconcile calls advance with the
    closed node id + its project (AC1-HP / AC1-RACE: advance runs AFTER close)."""
    graph_path, _ = cli_env
    _make_graph(
        graph_path,
        [_node("ab-adv", pr_number=700, project="fno", cwd="/proj/fno")],
    )
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({700: "MERGED"}))

    calls = []
    import fno.backlog.advance as advmod
    from pathlib import Path as _P

    def _capture(**kwargs):
        # The node must already be closed when advance reads it (AC1-RACE).
        node = next(e for e in _read_entries(graph_path) if e["id"] == "ab-adv")
        calls.append({"closed": kwargs.get("closed_node_id"),
                      "project": kwargs.get("project"),
                      "project_root": kwargs.get("project_root"),
                      "node_done": node.get("completed_at") is not None})
        return advmod.AdvanceResult("skipped", "advance_skipped", reason="disabled")

    monkeypatch.setattr(advmod, "advance", _capture)

    result = runner.invoke(app, ["backlog", "reconcile"])
    assert result.exit_code == 0, result.output
    # Fix (codex P2): advance is resolved against the CLOSED node's project root
    # (its cwd), not the reconcile's cwd.
    assert calls == [{
        "closed": "ab-adv", "project": "fno",
        "project_root": _P("/proj/fno"), "node_done": True,
    }]


def test_reconcile_advances_cascade_closed_parent_epic(cli_env, monkeypatch):
    """x-33b2 (codex P1): when closing a child cascade-closes its parent epic, a
    node blocked_by the EPIC must be dispatched too - else it stalls. Reconcile
    runs the full auto-continue path (advance + advance_dependents +
    dispatch_reconcile_for_blocker) for the cascade-closed epic, so its
    dependent's dispatch follows the epic's edges, not just the child's."""
    graph_path, _ = cli_env
    _make_graph(
        graph_path,
        [
            _node("ab-epicclose", project="web", cwd="/proj/web"),
            _node("ab-lastkid01", pr_number=810, project="fno", cwd="/proj/fno",
                  parent="ab-epicclose"),
            # A dependent of the EPIC (not the child): only reachable if advance's
            # dependent path runs for the cascade-closed parent.
            _node("ab-epicdep01", project="api", blocked_by=["ab-epicclose"]),
        ],
    )
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({810: "MERGED"}))

    adv_ids, dep_ids, recon_ids = [], [], []
    import fno.backlog.advance as advmod
    import fno.backlog.reconcile_dispatch as recdisp

    def _cap_adv(**kw):
        adv_ids.append(kw.get("closed_node_id"))
        return advmod.AdvanceResult("skipped", "advance_skipped", reason="disabled")

    monkeypatch.setattr(advmod, "advance", _cap_adv)
    monkeypatch.setattr(advmod, "advance_dependents",
                        lambda **kw: dep_ids.append(kw.get("closed_node_id")) or [])
    monkeypatch.setattr(recdisp, "dispatch_reconcile_for_blocker",
                        lambda **kw: recon_ids.append(kw.get("closed_node_id")))

    result = runner.invoke(app, ["backlog", "reconcile"])
    assert result.exit_code == 0, result.output
    # The cascade-closed epic is run through the FULL auto-continue path, so its
    # own dependent (ab-epicdep01) is reachable via the parent's edges.
    assert "ab-epicclose" in adv_ids        # same-project `next`
    assert "ab-epicclose" in dep_ids        # cross-project dependents (the epicdep)
    assert "ab-epicclose" in recon_ids      # contract de-stub
    assert "ab-lastkid01" in adv_ids        # the child still advanced too
    # The epic actually closed (cascade fired); the dependent is now unblocked.
    epic = next(e for e in _read_entries(graph_path) if e["id"] == "ab-epicclose")
    assert epic["completed_at"] is not None


def test_reconcile_self_heals_pre_existing_stranded_epic(cli_env, monkeypatch):
    """x-33b2 (codex P2 migration): an epic whose children were ALL completed
    before the cascade shipped gets no future child-close event, and containers
    are hidden from next/ready - so it would strand forever. Reconcile self-heals
    it (closes it + dispatches its dependents) even with NO merged-PR drift."""
    graph_path, _ = cli_env
    _make_graph(
        graph_path,
        [
            # Open epic, but BOTH children already done -> stranded, no PR drift.
            _node("ab-strandep0", project="web", cwd="/proj/web"),
            _node("ab-donekid01", project="web", parent="ab-strandep0",
                  completed_at="2026-01-01T00:00:00Z"),
            _node("ab-donekid02", project="web", parent="ab-strandep0",
                  completed_at="2026-01-02T00:00:00Z"),
            _node("ab-stranddep", project="api", blocked_by=["ab-strandep0"]),
        ],
    )
    # No merged-PR drift: scan finds nothing closeable.
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({}))

    dep_ids = []
    import fno.backlog.advance as advmod
    import fno.backlog.reconcile_dispatch as recdisp
    monkeypatch.setattr(advmod, "advance",
                        lambda **kw: advmod.AdvanceResult("skipped", "advance_skipped", reason="disabled"))
    monkeypatch.setattr(advmod, "advance_dependents",
                        lambda **kw: dep_ids.append(kw.get("closed_node_id")) or [])
    monkeypatch.setattr(recdisp, "dispatch_reconcile_for_blocker", lambda **kw: None)

    result = runner.invoke(app, ["backlog", "reconcile"])
    assert result.exit_code == 0, result.output
    epic = next(e for e in _read_entries(graph_path) if e["id"] == "ab-strandep0")
    assert epic["completed_at"] is not None              # self-healed closed
    assert "auto-closed" in (epic.get("completion_note") or "")
    assert "ab-strandep0" in dep_ids                     # its dependent dispatched


def test_reconcile_node_scoped_skips_global_epic_sweep(cli_env, monkeypatch):
    """codex P2: `reconcile --node <id>` must not close/dispatch UNRELATED stranded
    epics - the global self-heal sweep is suppressed on a node-scoped run."""
    graph_path, _ = cli_env
    _make_graph(
        graph_path,
        [
            # An unrelated stranded all-done epic that must be left alone.
            _node("ab-otherepic", project="web"),
            _node("ab-otherkid1", project="web", parent="ab-otherepic",
                  completed_at="2026-01-01T00:00:00Z"),
            # The targeted node (has a merged PR so the node-scoped scan closes it).
            _node("ab-target001", pr_number=900, project="fno"),
        ],
    )
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({900: "MERGED"}))
    import fno.backlog.advance as advmod
    monkeypatch.setattr(advmod, "advance",
                        lambda **kw: advmod.AdvanceResult("skipped", "advance_skipped", reason="disabled"))

    result = runner.invoke(app, ["backlog", "reconcile", "--node", "ab-target001"])
    assert result.exit_code == 0, result.output
    nodes = {e["id"]: e for e in _read_entries(graph_path)}
    assert nodes["ab-target001"]["completed_at"] is not None   # target closed
    assert nodes["ab-otherepic"].get("completed_at") is None   # unrelated epic untouched


def test_reconcile_dry_run_previews_stranded_epics(cli_env, monkeypatch):
    """codex P3: --dry-run previews the stranded all-done epics that WOULD heal,
    and mutates nothing."""
    graph_path, _ = cli_env
    _make_graph(
        graph_path,
        [
            _node("ab-strandep0", project="web"),
            _node("ab-donekid01", project="web", parent="ab-strandep0",
                  completed_at="2026-01-01T00:00:00Z"),
        ],
    )
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({}))

    result = runner.invoke(app, ["backlog", "reconcile", "--dry-run", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["healed_epics"] == ["ab-strandep0"]         # previewed
    # Nothing mutated.
    epic = next(e for e in _read_entries(graph_path) if e["id"] == "ab-strandep0")
    assert epic.get("completed_at") is None


def test_reconcile_dry_run_preview_includes_cascade_closed_parent(cli_env, monkeypatch):
    """codex P2: the --dry-run heal preview must include a parent that a real run
    would CASCADE-close from a closeable last child - not just the pre-close
    strandable set (the parent is NOT strandable until its child closes)."""
    graph_path, _ = cli_env
    _make_graph(
        graph_path,
        [
            _node("ab-cascadeep", project="web"),
            # Last child: a merged PR makes it closeable; it is NOT done yet, so
            # the parent is not strandable pre-close - only the simulation reveals it.
            _node("ab-lastchild", pr_number=950, project="fno", parent="ab-cascadeep"),
        ],
    )
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({950: "MERGED"}))

    result = runner.invoke(app, ["backlog", "reconcile", "--dry-run", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["healed_epics"] == ["ab-cascadeep"]   # cascade-close previewed
    # Nothing mutated.
    nodes = {e["id"]: e for e in _read_entries(graph_path)}
    assert nodes["ab-cascadeep"].get("completed_at") is None
    assert nodes["ab-lastchild"].get("completed_at") is None


def test_reconcile_advance_failure_does_not_abort_sweep(cli_env, monkeypatch):
    """Task 2.1: a raising advance is non-fatal - the node still closes and the
    reconcile run still exits 0 (the sweep is never wedged by auto-continue)."""
    graph_path, _ = cli_env
    _make_graph(graph_path, [_node("ab-nf", pr_number=750, project="fno")])
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({750: "MERGED"}))

    import fno.backlog.advance as advmod

    def _boom(**kwargs):
        raise RuntimeError("daemon down mid-advance")

    monkeypatch.setattr(advmod, "advance", _boom)

    result = runner.invoke(app, ["backlog", "reconcile"])
    assert result.exit_code == 0, result.output
    node = next(e for e in _read_entries(graph_path) if e["id"] == "ab-nf")
    assert node["completed_at"] is not None  # close survived the advance failure


def test_reconcile_query_failure_exits_nonzero(cli_env, monkeypatch):
    """A gh query failure is surfaced to stderr and forces a non-zero exit."""
    graph_path, _ = cli_env
    _make_graph(graph_path, [_node("ab-fail", pr_number=800)])

    def _boom(number, repo=None, cwd=None):
        raise ReconcileError("gh auth required")

    monkeypatch.setattr(rec, "query_pr_merge_state", _boom)

    result = runner.invoke(app, ["backlog", "reconcile"])
    assert result.exit_code == 4, result.output
    # Node left open; failure reported.
    node = next(e for e in _read_entries(graph_path) if e["id"] == "ab-fail")
    assert node["completed_at"] is None
    assert "ab-fail" in result.output


def test_reconcile_skips_node_closed_between_scan_and_lock(cli_env, monkeypatch):
    """If a record's node is already done at mutation time (raced close), it is
    not re-stamped, not given a sentinel, and not reported as closed."""
    graph_path, sentinel_dir = cli_env
    # Node is already done on disk; we force scan to yield it as a closeable
    # candidate anyway to simulate the scan-then-lock race window.
    _make_graph(graph_path, [_node("ab-race", pr_number=900,
                                    completed_at="2026-01-01T00:00:00+00:00")])
    from fno.graph._reconcile import MergeDriftRecord
    raced = MergeDriftRecord(
        node_id="ab-race", plan_path=None, pr_number=900,
        pr_url="u900", pr_state="MERGED", merged_at="2026-05-24T00:00:00Z",
    )
    monkeypatch.setattr(rec, "scan_merge_drift", lambda *a, **k: [raced])

    result = runner.invoke(app, ["backlog", "reconcile"])
    assert result.exit_code == 0, result.output
    assert "Closed 0" in result.output
    assert not (sentinel_dir / "ab-race.json").exists()


def test_reconcile_stamps_plan(cli_env, monkeypatch):
    """A closed node with plan_path triggers the best-effort plan stamp."""
    graph_path, _ = cli_env
    _make_graph(graph_path, [_node("ab-plan", pr_number=700, plan_path="plans/p.md")])
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({700: "MERGED"}))

    calls: list[tuple] = []
    import fno.graph.cli as gcli

    def _stamp_ok(p, **kw):
        calls.append((p, kw))
        return True

    monkeypatch.setattr(gcli, "_stamp_and_graduate_plan", _stamp_ok)

    result = runner.invoke(app, ["backlog", "reconcile", "--json"])
    assert result.exit_code == 0, result.output
    # The helper is called with the plan_path positionally and the merged PR url
    # threaded as a kwarg so the plan is stamped shipped, not just graduated
    # (ab-bd9f476c).
    assert len(calls) == 1
    assert calls[0][0] == "plans/p.md"
    assert calls[0][1]["url"] == "https://github.com/o/r/pull/700"
    payload = json.loads(result.output)
    assert payload["closed"][0]["plan_stamped"] is True


def test_reconcile_plan_stamp_failure_reports_false(cli_env, monkeypatch):
    """When the plan-stamp helper no-ops/fails, plan_stamped must report False."""
    graph_path, _ = cli_env
    _make_graph(graph_path, [_node("ab-nostamp", pr_number=750, plan_path="plans/q.md")])
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({750: "MERGED"}))

    import fno.graph.cli as gcli
    monkeypatch.setattr(gcli, "_stamp_and_graduate_plan", lambda p, **k: False)

    result = runner.invoke(app, ["backlog", "reconcile", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["closed"][0]["plan_stamped"] is False


def test_reconcile_real_stamp_marks_never_shipped_plan_done(cli_env, monkeypatch, tmp_path):
    """End-to-end (no helper monkeypatch): a merged node whose plan never went
    through the ship gate gets stamped shipped->done, not a graduate no-op
    (ab-bd9f476c)."""
    graph_path, _ = cli_env
    plan = tmp_path / "never-shipped.md"
    plan.write_text("---\ntitle: t\nstatus: ready\n---\n\nbody\n")
    _make_graph(graph_path, [_node("ab-rstamp", pr_number=800, plan_path=str(plan))])
    monkeypatch.setattr(rec, "query_pr_merge_state", _stub_query({800: "MERGED"}))

    result = runner.invoke(app, ["backlog", "reconcile", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["closed"][0]["plan_stamped"] is True

    text = plan.read_text()
    assert "status: done" in text  # graduate flipped shipped->done (1 url >= 1)
    assert "shipped_at:" in text
    assert "pull/800" in text
