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
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", graph_path.parent / "graph.lock")
    monkeypatch.setattr(gc, "GRAPH_MD", graph_path.parent / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", graph_path.parent / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", graph_path)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", graph_path.parent / "graph.lock")


def _make_graph(path: Path, entries: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


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
    assert node["_status"] == "done"
    assert (sentinel_dir / "ab-hp.json").exists()

    # Second run: node now done -> scan skips it -> completed_at unchanged.
    first_ts = node["completed_at"]
    result2 = runner.invoke(app, ["backlog", "reconcile"])
    assert result2.exit_code == 0, result2.output
    node2 = next(e for e in _read_entries(graph_path) if e["id"] == "ab-hp")
    assert node2["completed_at"] == first_ts


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
        [_node("ab-adv", pr_number=700, project="fno", cwd="/proj/abilities")],
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
        "project_root": _P("/proj/abilities"), "node_done": True,
    }]


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
