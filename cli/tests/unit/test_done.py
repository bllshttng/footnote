"""Unit tests for `fno done` command.

Uses typer.testing.CliRunner with a monkey-patched GRAPH_JSON pointing at a
temp file, identical to the pattern in tests/integration/test_graph_cli.py.
subprocess calls (git, gh) are stubbed via monkeypatch.setattr at module level
so we never hit the real filesystem or GitHub.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


# -- fixtures --


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    """A fresh empty graph.json routed to the done command's code path.

    Also routes LEDGER_JSON to an empty file by default so rollup sees nothing
    unless a test explicitly writes ledger fixture data.
    """
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    # Remove any ambient CLAUDECODE_SESSION_ID so tests that don't set it
    # don't accidentally inherit a real session id from the host.
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    return g


@pytest.fixture(autouse=True)
def merged_pr(monkeypatch):
    """Default every `--pr` close to a MERGED PR.

    `fno done --pr N` now demands gh-resolved merge evidence, so without a stub
    every close in this module would exit 4 (gh outage). Tests that exercise the
    gate itself override this with their own stub.
    """
    import fno.done.cli as done_cli
    from fno.graph._reconcile import PrMergeState

    def _merged(pr_number, **kwargs):
        return PrMergeState(
            number=pr_number,
            state="MERGED",
            url=f"https://github.com/o/r/pull/{pr_number}",
            merged_at="2026-01-01T00:00:00Z",
        )

    monkeypatch.setattr(done_cli, "_gh_query", _merged)


@pytest.fixture
def tmp_ledger(tmp_path) -> Path:
    """The ledger path tmp_graph routes to. Tests that want rollup data write
    here via _seed_ledger()."""
    return tmp_path / "ledger.json"


def _seed_ledger(ledger: Path, entries: list[dict]) -> None:
    ledger.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _read(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


def _stub_subprocess(
    monkeypatch,
    *,
    branch: str | None = None,
    pr_view_json: str | None = None,
    pr_view_rc: int = 1,
    repo_name: str | None = None,
    repo_rc: int = 1,
    origin: str | None = "https://github.com/org/repo.git",
):
    """Replace subprocess.run for fno.done.cli with scripted responses.

    - git branch --show-current -> returns `branch` (or empty if None)
    - git remote get-url origin -> returns `origin` (rc 1 when None)
    - gh pr view ... -> stdout = pr_view_json, rc = pr_view_rc
    - gh repo view ... -> stdout = repo_name, rc = repo_rc

    `origin` defaults to a parseable GitHub remote: the pr_url derivation now
    resolves the slug the way the reader does (origin first, gh second), so a
    checkout with no remote at all is the exception, not the baseline.
    """
    from fno.done import cli as done_cli

    class _Result:
        def __init__(self, stdout: str = "", rc: int = 0):
            self.stdout = stdout
            self.returncode = rc

    def fake_run(cmd, **kwargs):
        if not cmd:
            return _Result("", 1)
        if cmd[0] == "git" and "branch" in cmd:
            return _Result((branch or "") + "\n", 0 if branch else 0)
        if cmd[0] == "git" and "remote" in cmd:
            return _Result((origin or "") + "\n", 0 if origin else 1)
        if cmd[0] == "gh" and "pr" in cmd:
            return _Result((pr_view_json or "") + "\n", pr_view_rc)
        if cmd[0] == "gh" and "repo" in cmd:
            return _Result((repo_name or "") + "\n", repo_rc)
        return _Result("", 1)

    monkeypatch.setattr(done_cli.subprocess, "run", fake_run)


# -- tests --


def test_scenario1_hp_abi_done_noargs_auto_detects_pr(tmp_graph, monkeypatch):
    """Scenario 1 (HP): no args + matching branch + gh pr resolves everything."""
    _seed(tmp_graph, [{
        "id": "ab-tot00001",
        "title": "Implement tot init command",
        "status": "ready",
        "domain": "code",
    }])
    _stub_subprocess(
        monkeypatch,
        branch="feat/tot-init-first-run-ux",
        pr_view_json="42 https://github.com/org/repo/pull/42",
        pr_view_rc=0,
    )
    result = runner.invoke(app, ["done"])
    assert result.exit_code == 0, result.stdout
    entry = next(e for e in _read(tmp_graph) if e["id"] == "ab-tot00001")
    assert entry["status"] == "done"
    assert entry["completed_at"]
    assert entry["pr_number"] == 42
    assert entry["pr_url"] == "https://github.com/org/repo/pull/42"
    assert entry["merge_status"] == "merged"


def test_scenario2_hp_abi_done_id_pr_explicit(tmp_graph, monkeypatch):
    """Scenario 2 (HP): explicit id + --pr derives URL via gh repo view."""
    _seed(tmp_graph, [{
        "id": "ab-54e461b6",
        "title": "Whatever",
        "status": "ready",
        "domain": "code",
    }])
    _stub_subprocess(
        monkeypatch,
        branch="main",
        pr_view_rc=1,  # no PR on current branch - forces fallback
        repo_name="bllshttng/footnote",
        repo_rc=0,
        origin=None,  # no remote: the gh leg of the chain carries the slug
    )
    result = runner.invoke(app, ["done", "ab-54e461b6", "--pr", "9"])
    assert result.exit_code == 0, result.stdout
    entry = _read(tmp_graph)[0]
    assert entry["pr_number"] == 9
    assert entry["pr_url"] == "https://github.com/bllshttng/footnote/pull/9"
    assert entry["merge_status"] == "merged"
    assert entry["status"] == "done"


def test_scenario3_hp_non_code_link(tmp_graph, monkeypatch):
    """Scenario 3 (HP): non-code domain + --link sets artifact_url only."""
    _seed(tmp_graph, [{
        "id": "ab-q2000001",
        "title": "Q2 outreach research",
        "status": "ready",
        "domain": "research",
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1)
    result = runner.invoke(
        app,
        ["done", "Q2 outreach", "--link", "obsidian://vault/myvault/q2"],
    )
    assert result.exit_code == 0, result.stdout
    entry = _read(tmp_graph)[0]
    assert entry["status"] == "done"
    assert entry["artifact_url"] == "obsidian://vault/myvault/q2"
    assert entry["pr_number"] is None  # NOT auto-detected for non-code
    assert entry["pr_url"] is None


def test_scenario4_err_ambiguous_query_exits_2(tmp_graph, monkeypatch):
    """Scenario 4 (ERR): ambiguous fuzzy query exits 2, no mutation."""
    _seed(tmp_graph, [
        {"id": "ab-plan00001", "title": "Plan 01: one", "status": "ready", "domain": "code"},
        {"id": "ab-plan00002", "title": "Plan 02: two", "status": "ready", "domain": "code"},
    ])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1)
    before = _read(tmp_graph)
    result = runner.invoke(app, ["done", "plan"])
    assert result.exit_code == 2
    assert "ab-plan00001" in result.stdout or "ab-plan00001" in (result.stderr or "")
    assert _read(tmp_graph) == before  # no mutation


def test_scenario5_err_no_match_exits_2(tmp_graph, monkeypatch):
    """Scenario 5 (ERR): no match exits 2 with 'no match' message."""
    _seed(tmp_graph, [
        {"id": "ab-aa000001", "title": "something else", "status": "ready", "domain": "code"},
    ])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1)
    before = _read(tmp_graph)
    result = runner.invoke(app, ["done", "xyzzy"])
    assert result.exit_code == 2
    combined = result.stdout + (result.stderr or "")
    assert "no match" in combined.lower() or "no entry" in combined.lower()
    assert _read(tmp_graph) == before


def test_scenario6_err_non_code_no_artifact_exits_2(tmp_graph, monkeypatch):
    """Scenario 6 (ERR): non-code node without --link/--note/--pr exits 2."""
    _seed(tmp_graph, [{
        "id": "ab-res00001",
        "title": "Research task",
        "status": "ready",
        "domain": "research",
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1)
    before = _read(tmp_graph)
    result = runner.invoke(app, ["done", "ab-res00001"])
    assert result.exit_code == 2
    combined = result.stdout + (result.stderr or "")
    assert "research" in combined
    assert _read(tmp_graph) == before


def test_scenario7_edge_detached_head(tmp_graph, monkeypatch):
    """Scenario 7 (EDGE): detached HEAD, no query -> kind=none, exit 2."""
    _seed(tmp_graph, [{
        "id": "ab-aa000001",
        "title": "anything",
        "status": "ready",
        "domain": "code",
    }])
    _stub_subprocess(monkeypatch, branch=None, pr_view_rc=1)
    result = runner.invoke(app, ["done"])
    assert result.exit_code == 2


def test_scenario8_edge_pr_explicit_gh_missing(tmp_graph, monkeypatch):
    """Scenario 8 (EDGE): --pr explicit and gh unavailable - origin carries the url.

    The writer resolves the slug with the reader's own chain, so an absent or
    unauthenticated gh no longer costs the pr_url.
    """
    _seed(tmp_graph, [{
        "id": "ab-xx000001",
        "title": "T",
        "status": "ready",
        "domain": "code",
    }])
    _stub_subprocess(
        monkeypatch,
        branch="main",
        pr_view_rc=1,
        repo_rc=1,  # gh repo view fails
    )
    result = runner.invoke(app, ["done", "ab-xx000001", "--pr", "42"])
    assert result.exit_code == 0, result.stdout
    entry = _read(tmp_graph)[0]
    assert entry["pr_number"] == 42
    assert entry["merge_status"] == "merged"
    assert entry["pr_url"] == "https://github.com/org/repo/pull/42"


def test_pr_stamp_refused_when_neither_origin_nor_gh_resolves(tmp_graph, monkeypatch):
    """No remote and no gh: refuse rather than write an unattributable number."""
    _seed(tmp_graph, [{
        "id": "ab-xx000001",
        "title": "T",
        "_status": "ready",
        "domain": "code",
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1, origin=None)
    result = runner.invoke(app, ["done", "ab-xx000001", "--pr", "42"])
    assert result.exit_code != 0
    assert _read(tmp_graph)[0].get("pr_number") is None


def test_help_lists_done_command(tmp_graph, monkeypatch):
    """Sanity: `fno done --help` shows usage."""
    result = runner.invoke(app, ["done", "--help"])
    assert result.exit_code == 0
    assert "done" in result.stdout.lower()


def test_note_is_preserved(tmp_graph, monkeypatch):
    """--note sets completion_note, doesn't touch other fields."""
    _seed(tmp_graph, [{
        "id": "ab-tr000001",
        "title": "trade 2026-04-22 AAPL",
        "status": "ready",
        "domain": "trading",
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1)
    result = runner.invoke(
        app,
        ["done", "ab-tr000001", "--note", "closed AAPL $180 strangle"],
    )
    assert result.exit_code == 0, result.stdout
    entry = _read(tmp_graph)[0]
    assert entry["status"] == "done"
    assert entry["completion_note"] == "closed AAPL $180 strangle"
    assert entry["pr_number"] is None
    assert entry["artifact_url"] is None


def test_link_and_note_both_set(tmp_graph, monkeypatch):
    """--link and --note together both write."""
    _seed(tmp_graph, [{
        "id": "ab-de000001",
        "title": "Design mockup",
        "status": "ready",
        "domain": "design",
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1)
    result = runner.invoke(
        app,
        [
            "done",
            "ab-de000001",
            "--link",
            "https://figma.com/design/xyz",
            "--note",
            "v2 mockup approved",
        ],
    )
    assert result.exit_code == 0, result.stdout
    entry = _read(tmp_graph)[0]
    assert entry["artifact_url"] == "https://figma.com/design/xyz"
    assert entry["completion_note"] == "v2 mockup approved"


def test_completed_at_is_iso8601(tmp_graph, monkeypatch):
    """completed_at is populated with an ISO 8601 timestamp."""
    _seed(tmp_graph, [{
        "id": "ab-ts000001",
        "title": "T",
        "status": "ready",
        "domain": "code",
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1)
    result = runner.invoke(app, ["done", "ab-ts000001", "--pr", "1"])
    assert result.exit_code == 0, result.stdout
    entry = _read(tmp_graph)[0]
    ts = entry["completed_at"]
    assert ts
    # ISO 8601: 2026-04-22T...
    assert "T" in ts
    assert ts[:4].isdigit()  # year


# -- Ledger rollup tests --


def test_rollup_populates_cost_usd_from_ledger(tmp_graph, tmp_ledger, monkeypatch):
    """Ledger entry with matching plan_path -> cost_usd and cost_sessions filled."""
    _seed(tmp_graph, [{
        "id": "ab-roll0001",
        "title": "Rollup target",
        "status": "ready",
        "domain": "code",
        "plan_path": "/repo/plans/2026-04-22-thing",
        "cost_usd": None,
        "cost_sessions": [],
    }])
    _seed_ledger(tmp_ledger, [{
        "plan_path": "/repo/plans/2026-04-22-thing",
        "sessions": ["sess-aaa", "sess-bbb"],
        "cost_usd": 9.50,
        "completed": "2026-04-22T19:00:00Z",
        "points": 8,
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1)
    result = runner.invoke(app, ["done", "ab-roll0001", "--pr", "42"])
    assert result.exit_code == 0, result.stdout
    entry = _read(tmp_graph)[0]
    assert entry["cost_usd"] == 9.5
    # Two sessions -> two cost_sessions rows, split evenly.
    assert len(entry["cost_sessions"]) == 2
    cost_values = sorted(s["cost_usd"] for s in entry["cost_sessions"])
    assert cost_values == [4.75, 4.75]
    session_ids = {s["session_id"] for s in entry["cost_sessions"]}
    assert session_ids == {"sess-aaa", "sess-bbb"}
    assert entry["points"] == 8


def test_rollup_session_id_from_latest_ledger_entry(tmp_graph, tmp_ledger, monkeypatch):
    """session_id picks the last UUID from the most-recent-completed ledger row."""
    _seed(tmp_graph, [{
        "id": "ab-late0001",
        "title": "T",
        "status": "ready",
        "domain": "code",
        "plan_path": "/p",
    }])
    _seed_ledger(tmp_ledger, [
        {"plan_path": "/p", "sessions": ["old-sid"], "cost_usd": 1.0,
         "completed": "2026-04-20T12:00:00Z"},
        {"plan_path": "/p", "sessions": ["mid-sid", "latest-sid"], "cost_usd": 2.0,
         "completed": "2026-04-22T12:00:00Z"},
    ])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1)
    result = runner.invoke(app, ["done", "ab-late0001", "--pr", "1"])
    assert result.exit_code == 0, result.stdout
    entry = _read(tmp_graph)[0]
    assert entry["session_id"] == "latest-sid"
    # Both ledger entries contribute to cost.
    assert entry["cost_usd"] == 3.0


def test_rollup_env_session_id_overrides_ledger(tmp_graph, tmp_ledger, monkeypatch):
    """$CLAUDECODE_SESSION_ID wins when present (this session attributes the work)."""
    _seed(tmp_graph, [{
        "id": "ab-env00001",
        "title": "T",
        "status": "ready",
        "domain": "code",
        "plan_path": "/p",
    }])
    _seed_ledger(tmp_ledger, [{
        "plan_path": "/p",
        "sessions": ["ledger-sid"],
        "cost_usd": 1.0,
        "completed": "2026-04-22T12:00:00Z",
    }])
    monkeypatch.setenv("CLAUDECODE_SESSION_ID", "current-claude-sess")
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1)
    result = runner.invoke(app, ["done", "ab-env00001", "--pr", "1"])
    assert result.exit_code == 0, result.stdout
    entry = _read(tmp_graph)[0]
    assert entry["session_id"] == "current-claude-sess"


def test_rollup_empty_ledger_leaves_fields_null(tmp_graph, tmp_ledger, monkeypatch):
    """Empty ledger -> fno done does not null-out or partial-fill rollup fields."""
    _seed(tmp_graph, [{
        "id": "ab-emp00001",
        "title": "T",
        "status": "ready",
        "domain": "code",
        "plan_path": "/unknown-plan",
        "cost_usd": None,
        "cost_sessions": [],
        "points": None,
        "session_id": None,
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1)
    result = runner.invoke(app, ["done", "ab-emp00001", "--pr", "1"])
    assert result.exit_code == 0, result.stdout
    entry = _read(tmp_graph)[0]
    assert entry["cost_usd"] is None
    assert entry["cost_sessions"] == []
    assert entry["points"] is None
    assert entry["session_id"] is None


def test_rollup_preserves_existing_session_id(tmp_graph, tmp_ledger, monkeypatch):
    """Pre-existing session_id (e.g. set by --locked-by) is not overwritten."""
    _seed(tmp_graph, [{
        "id": "ab-pre00001",
        "title": "T",
        "status": "ready",
        "domain": "code",
        "plan_path": "/p",
        "session_id": "sticky-session",
    }])
    _seed_ledger(tmp_ledger, [{
        "plan_path": "/p",
        "sessions": ["would-overwrite"],
        "cost_usd": 1.0,
        "completed": "2026-04-22T12:00:00Z",
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1)
    result = runner.invoke(app, ["done", "ab-pre00001", "--pr", "1"])
    assert result.exit_code == 0, result.stdout
    entry = _read(tmp_graph)[0]
    assert entry["session_id"] == "sticky-session"


def test_rollup_cost_sessions_dedups_across_runs(tmp_graph, tmp_ledger, monkeypatch):
    """Running fno done twice does not double-count cost_sessions."""
    _seed(tmp_graph, [{
        "id": "ab-dup00001",
        "title": "T",
        "status": "ready",
        "domain": "code",
        "plan_path": "/p",
    }])
    _seed_ledger(tmp_ledger, [{
        "plan_path": "/p",
        "sessions": ["sess-one"],
        "cost_usd": 5.0,
        "completed": "2026-04-22T12:00:00Z",
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1)
    runner.invoke(app, ["done", "ab-dup00001", "--pr", "1"])
    # Invoke again - should not double-insert.
    result = runner.invoke(app, ["done", "ab-dup00001", "--pr", "1"])
    assert result.exit_code == 0, result.stdout
    entry = _read(tmp_graph)[0]
    assert len(entry["cost_sessions"]) == 1
    assert entry["cost_usd"] == 5.0


def test_rollup_handles_ledger_entry_with_no_sessions(tmp_graph, tmp_ledger, monkeypatch):
    """Ledger entry with missing/null `sessions` still records the cost row."""
    _seed(tmp_graph, [{
        "id": "ab-nos00001",
        "title": "T",
        "status": "ready",
        "domain": "code",
        "plan_path": "/p",
    }])
    _seed_ledger(tmp_ledger, [{
        "plan_path": "/p",
        "sessions": None,  # Some old ledger entries have this shape.
        "cost_usd": 3.0,
        "completed": "2026-04-22T12:00:00Z",
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1)
    result = runner.invoke(app, ["done", "ab-nos00001", "--pr", "1"])
    assert result.exit_code == 0, result.stdout
    entry = _read(tmp_graph)[0]
    assert entry["cost_usd"] == 3.0
    assert len(entry["cost_sessions"]) == 1
    assert entry["cost_sessions"][0]["session_id"] is None


# -- Backfill tests --


def test_backfill_single_node_fills_without_flipping_status(tmp_graph, tmp_ledger, monkeypatch):
    """--backfill on an already-done node only fills rollup, does not touch status."""
    _seed(tmp_graph, [{
        "id": "ab-bfl00001",
        "title": "T",
        "status": "done",
        "completed_at": "2026-04-20T10:00:00Z",  # prior completion
        "domain": "code",
        "plan_path": "/p",
    }])
    _seed_ledger(tmp_ledger, [{
        "plan_path": "/p",
        "sessions": ["sess-x"],
        "cost_usd": 7.5,
        "completed": "2026-04-22T12:00:00Z",
        "points": 3,
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1)
    result = runner.invoke(app, ["done", "ab-bfl00001", "--backfill"])
    assert result.exit_code == 0, result.stdout
    entry = _read(tmp_graph)[0]
    # Status + completed_at untouched.
    assert entry["status"] == "done"
    assert entry["completed_at"] == "2026-04-20T10:00:00Z"
    # Rollup fields now present.
    assert entry["cost_usd"] == 7.5
    assert entry["points"] == 3
    assert entry["session_id"] == "sess-x"


def test_backfill_sweep_all_done_nodes(tmp_graph, tmp_ledger, monkeypatch):
    """`fno done --backfill` with no id sweeps every node with status=done."""
    _seed(tmp_graph, [
        {"id": "ab-d1000001", "title": "done one", "status": "done",
         "domain": "code", "plan_path": "/p1"},
        {"id": "ab-d2000002", "title": "done two", "status": "done",
         "domain": "code", "plan_path": "/p2"},
        {"id": "ab-r1000003", "title": "not done", "status": "ready",
         "domain": "code", "plan_path": "/p3"},
    ])
    _seed_ledger(tmp_ledger, [
        {"plan_path": "/p1", "sessions": ["s1"], "cost_usd": 1.0,
         "completed": "2026-04-22T12:00:00Z"},
        {"plan_path": "/p2", "sessions": ["s2"], "cost_usd": 2.0,
         "completed": "2026-04-22T13:00:00Z"},
        {"plan_path": "/p3", "sessions": ["s3"], "cost_usd": 3.0,
         "completed": "2026-04-22T14:00:00Z"},
    ])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1)
    result = runner.invoke(app, ["done", "--backfill"])
    assert result.exit_code == 0, result.stdout
    entries = {e["id"]: e for e in _read(tmp_graph)}
    # done nodes got rollup
    assert entries["ab-d1000001"]["cost_usd"] == 1.0
    assert entries["ab-d2000002"]["cost_usd"] == 2.0
    # ready node skipped - still null
    assert entries["ab-r1000003"]["cost_usd"] is None


def test_backfill_no_done_nodes_noop(tmp_graph, tmp_ledger, monkeypatch):
    """Sweep with no done nodes reports cleanly and does not crash."""
    _seed(tmp_graph, [
        {"id": "ab-rd000001", "title": "ready", "status": "ready",
         "domain": "code", "plan_path": "/p"},
    ])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1)
    result = runner.invoke(app, ["done", "--backfill"])
    assert result.exit_code == 0, result.stdout
    assert "no done nodes" in result.stdout.lower()


def test_backfill_reports_counts(tmp_graph, tmp_ledger, monkeypatch):
    """Backfill output tells the user how many nodes it updated."""
    _seed(tmp_graph, [{
        "id": "ab-cnt00001",
        "title": "T",
        "status": "done",
        "completed_at": "2026-04-20T00:00:00Z",
        "domain": "code",
        "plan_path": "/p",
    }])
    _seed_ledger(tmp_ledger, [{
        "plan_path": "/p",
        "sessions": ["s1"],
        "cost_usd": 1.5,
        "completed": "2026-04-22T12:00:00Z",
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1)
    result = runner.invoke(app, ["done", "ab-cnt00001", "--backfill"])
    assert result.exit_code == 0, result.stdout
    assert "updated" in result.stdout.lower() or "ab-cnt00001" in result.stdout


def test_rollup_tags_in_normal_output(tmp_graph, tmp_ledger, monkeypatch):
    """Normal `fno done` output mentions cost when rollup fired."""
    _seed(tmp_graph, [{
        "id": "ab-tag00001",
        "title": "T",
        "status": "ready",
        "domain": "code",
        "plan_path": "/p",
    }])
    _seed_ledger(tmp_ledger, [{
        "plan_path": "/p",
        "sessions": ["s1"],
        "cost_usd": 12.34,
        "completed": "2026-04-22T12:00:00Z",
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1)
    result = runner.invoke(app, ["done", "ab-tag00001", "--pr", "1"])
    assert result.exit_code == 0, result.stdout
    # Output should include $ somewhere from the cost tag.
    assert "$" in result.stdout or "12.34" in result.stdout


# -- AC4 stderr surface tests --


def _stub_subprocess_with_stderr(
    monkeypatch,
    *,
    branch: str | None = "feat/thing",
    pr_view_stdout: str = "",
    pr_view_rc: int = 1,
    pr_view_stderr: str = "",
    pr_view_raises: type[Exception] | None = None,
    origin: str | None = "https://github.com/org/repo.git",
):
    """Subprocess stub that exposes stderr on the gh pr view result.

    Distinct from _stub_subprocess so it can surface stderr and raise on demand
    without changing the existing helper's signature.
    """
    from fno.done import cli as done_cli

    class _Result:
        def __init__(
            self,
            stdout: str = "",
            rc: int = 0,
            stderr: str = "",
        ):
            self.stdout = stdout
            self.returncode = rc
            self.stderr = stderr

    def fake_run(cmd, **kwargs):
        if not cmd:
            return _Result("", 1)
        if cmd[0] == "git" and "branch" in cmd:
            return _Result((branch or "") + "\n", 0, "")
        if cmd[0] == "git" and "remote" in cmd:
            return _Result((origin or "") + "\n", 0 if origin else 1, "")
        if cmd[0] == "gh" and "pr" in cmd:
            if pr_view_raises is not None:
                raise pr_view_raises("stubbed exception")
            return _Result(pr_view_stdout, pr_view_rc, pr_view_stderr)
        # gh repo view and everything else: fail silently
        return _Result("", 1, "")

    monkeypatch.setattr(done_cli.subprocess, "run", fake_run)


def test_ac4_hp_current_pr_success_no_stderr(tmp_graph, monkeypatch):
    """AC4-HP: rc=0, well-formed stdout -> (pr_number, url) returned, no stderr noise."""
    _seed(tmp_graph, [{
        "id": "ab-ac4hp001",
        "title": "HP target",
        "status": "ready",
        "domain": "code",
    }])
    _stub_subprocess_with_stderr(
        monkeypatch,
        branch="feat/thing",
        pr_view_stdout="123 https://github.com/x/y/pull/123",
        pr_view_rc=0,
        pr_view_stderr="",
    )
    result = runner.invoke(app, ["done", "ab-ac4hp001"])
    assert result.exit_code == 0, result.output
    entry = _read(tmp_graph)[0]
    assert entry["status"] == "done"
    assert entry["pr_number"] == 123
    assert entry["pr_url"] == "https://github.com/x/y/pull/123"
    # No gh failure noise on stderr.
    combined_err = result.stderr or ""
    assert "gh pr view failed" not in combined_err


def test_ac4_err_gh_fails_no_explicit_args_prints_stderr(tmp_graph, monkeypatch):
    """AC4-ERR: gh fails (rc=1) + no explicit --pr/--link/--note -> stderr surface."""
    _seed(tmp_graph, [{
        "id": "ab-ac4er001",
        "title": "ERR target",
        "status": "ready",
        "domain": "code",
    }])
    _stub_subprocess_with_stderr(
        monkeypatch,
        branch="feat/thing",
        pr_view_stdout="",
        pr_view_rc=1,
        pr_view_stderr="gh: not authenticated",
    )
    result = runner.invoke(app, ["done", "ab-ac4er001"], catch_exceptions=False)
    assert result.exit_code == 0, result.output
    # stderr must contain the prefix and the captured gh error.
    combined_err = result.output  # CliRunner mixes stderr into output by default
    assert "fno done: gh pr view failed:" in combined_err
    assert "not authenticated" in combined_err
    # Node is still marked done (no pr_number since gh failed).
    entry = _read(tmp_graph)[0]
    assert entry["status"] == "done"
    assert entry.get("pr_number") is None


def test_ac4_fr_explicit_pr_bypasses_current_pr(tmp_graph, monkeypatch):
    """AC4-FR: --pr explicit -> _current_pr never invoked; no stderr noise."""
    _seed(tmp_graph, [{
        "id": "ab-ac4fr001",
        "title": "FR target",
        "status": "ready",
        "domain": "code",
    }])
    current_pr_called = []

    from fno.done import cli as done_cli

    original_current_pr = done_cli._current_pr

    def spy_current_pr(*args, **kwargs):
        current_pr_called.append(True)
        return original_current_pr(*args, **kwargs)

    monkeypatch.setattr(done_cli, "_current_pr", spy_current_pr)
    # Also stub subprocess so git/gh don't actually run.
    _stub_subprocess_with_stderr(
        monkeypatch,
        branch="feat/thing",
        pr_view_rc=1,
        pr_view_stderr="gh: not authenticated",
    )
    result = runner.invoke(app, ["done", "ab-ac4fr001", "--pr", "123"], catch_exceptions=False)
    assert result.exit_code == 0, result.output
    # _current_pr must NOT have been called.
    assert not current_pr_called, "_current_pr was invoked despite --pr being passed"
    # No gh failure noise.
    assert "gh pr view failed" not in result.output


def test_ac4_edge_rc0_parse_failure_stays_silent(tmp_graph, monkeypatch):
    """AC4-EDGE: rc=0 but unparseable stdout -> (None, None) returned silently."""
    _seed(tmp_graph, [{
        "id": "ab-ac4ed001",
        "title": "EDGE target",
        "status": "ready",
        "domain": "code",
    }])
    _stub_subprocess_with_stderr(
        monkeypatch,
        branch="feat/thing",
        pr_view_stdout="garbage no parse",
        pr_view_rc=0,
        pr_view_stderr="",
    )
    result = runner.invoke(app, ["done", "ab-ac4ed001"], catch_exceptions=False)
    assert result.exit_code == 0, result.output
    # Parse failure is "no PR for this branch", not a subprocess error -> silent.
    assert "gh pr view failed" not in result.output
    # Node still marked done, but no pr_number.
    entry = _read(tmp_graph)[0]
    assert entry["status"] == "done"
    assert entry.get("pr_number") is None


# -- Operator-authority audit tag (ab-0b230fd8) --
#
# The top-level `fno done` verb mirrors `graph/cli.py::cmd_done`: when an
# operator holds a drive window, a fresh completion emits
# `backlog_done_operator_initiated` (source `backlog`) so the audit trail does
# not fork by verb. These tests parallel cmd_done's coverage in
# tests/integration/test_graph_cli.py.


def test_done_audit_tags_operator_when_driving(tmp_graph, monkeypatch):
    """AC1-HP: a fresh `fno done` during a drive window emits the operator tag."""
    from fno.agents import drive_authority as da

    captured: dict = {}
    monkeypatch.setattr(da, "is_drive_authority_active", lambda *a, **k: True)
    monkeypatch.setattr(
        da,
        "emit_operator_initiated",
        lambda action_type, **kw: captured.update(type=action_type, kw=kw),
    )
    _seed(tmp_graph, [{
        "id": "ab-drv00001",
        "title": "Drive completion",
        "status": "ready",
        "domain": "code",
    }])
    _stub_subprocess_with_stderr(monkeypatch, branch="main", pr_view_rc=0, pr_view_stdout="")
    result = runner.invoke(app, ["done", "ab-drv00001"])
    assert result.exit_code == 0, result.stdout
    assert _read(tmp_graph)[0]["status"] == "done"
    assert captured.get("type") == "backlog_done_operator_initiated"
    assert captured["kw"]["source"] == "backlog"
    assert captured["kw"]["task_id"] == "ab-drv00001"


def test_done_no_audit_tag_when_not_driving(tmp_graph, monkeypatch):
    """AC1-ERR: no drive window -> no operator tag; behavior unchanged."""
    from fno.agents import drive_authority as da

    calls = {"n": 0}
    monkeypatch.setattr(da, "is_drive_authority_active", lambda *a, **k: False)
    monkeypatch.setattr(
        da, "emit_operator_initiated", lambda *a, **k: calls.update(n=calls["n"] + 1)
    )
    _seed(tmp_graph, [{
        "id": "ab-ndr00001",
        "title": "No-drive completion",
        "status": "ready",
        "domain": "code",
    }])
    _stub_subprocess_with_stderr(monkeypatch, branch="main", pr_view_rc=0, pr_view_stdout="")
    result = runner.invoke(app, ["done", "ab-ndr00001"])
    assert result.exit_code == 0, result.stdout
    assert _read(tmp_graph)[0]["status"] == "done"
    assert calls["n"] == 0


def test_done_audit_tag_adds_no_stdout(tmp_graph, monkeypatch):
    """AC1-UI: the tag adds nothing to stdout; the completion line is identical
    whether or not a drive window is active."""
    from fno.agents import drive_authority as da

    monkeypatch.setattr(da, "emit_operator_initiated", lambda *a, **k: None)

    def _run(driving: bool, node_id: str) -> str:
        monkeypatch.setattr(da, "is_drive_authority_active", lambda *a, **k: driving)
        _seed(tmp_graph, [{
            "id": node_id, "title": "Same line", "status": "ready", "domain": "code",
        }])
        _stub_subprocess_with_stderr(monkeypatch, branch="main", pr_view_rc=0, pr_view_stdout="")
        r = runner.invoke(app, ["done", node_id])
        assert r.exit_code == 0, r.stdout
        return r.stdout

    out_inactive = _run(False, "ab-ui000001")
    out_active = _run(True, "ab-ui000001")
    assert out_active == out_inactive
    assert "backlog_done_operator_initiated" not in out_active


def test_done_no_tag_on_collision_even_when_driving(tmp_graph, monkeypatch):
    """AC1-EDGE: an already-done node hits the collision path (returns early) ->
    no completion occurred, so no operator tag even under an active drive."""
    from fno.agents import drive_authority as da

    calls = {"n": 0}
    monkeypatch.setattr(da, "is_drive_authority_active", lambda *a, **k: True)
    monkeypatch.setattr(
        da, "emit_operator_initiated", lambda *a, **k: calls.update(n=calls["n"] + 1)
    )
    _seed(tmp_graph, [{
        "id": "ab-col00001",
        "title": "Already done",
        "status": "done",
        "completed_at": "2026-04-20T10:00:00Z",
        "domain": "code",
    }])
    _stub_subprocess_with_stderr(monkeypatch, branch="main", pr_view_rc=0, pr_view_stdout="")
    result = runner.invoke(app, ["done", "ab-col00001"])
    assert result.exit_code == 0, result.stdout
    assert calls["n"] == 0


def test_done_no_tag_on_backfill_even_when_driving(tmp_graph, tmp_ledger, monkeypatch):
    """AC1-EDGE: `--backfill` is a rollup-only pass that returns before the
    completion path -> no operator tag even under an active drive."""
    from fno.agents import drive_authority as da

    calls = {"n": 0}
    monkeypatch.setattr(da, "is_drive_authority_active", lambda *a, **k: True)
    monkeypatch.setattr(
        da, "emit_operator_initiated", lambda *a, **k: calls.update(n=calls["n"] + 1)
    )
    _seed(tmp_graph, [{
        "id": "ab-bfd00001",
        "title": "Backfill target",
        "status": "done",
        "completed_at": "2026-04-20T10:00:00Z",
        "domain": "code",
        "plan_path": "/p",
    }])
    _seed_ledger(tmp_ledger, [{
        "plan_path": "/p", "sessions": ["s1"], "cost_usd": 1.0,
        "completed": "2026-04-22T12:00:00Z",
    }])
    _stub_subprocess(monkeypatch, branch="main", pr_view_rc=1, repo_rc=1)
    result = runner.invoke(app, ["done", "ab-bfd00001", "--backfill"])
    assert result.exit_code == 0, result.stdout
    assert calls["n"] == 0


def test_done_completes_even_when_audit_emit_raises(tmp_graph, monkeypatch):
    """AC1-FR: a raising emit (unwritable events.jsonl / non-serializable data)
    is swallowed -- the completion still succeeds and exits 0."""
    from fno.agents import drive_authority as da

    def _boom(*a, **k):
        raise OSError("events.jsonl unwritable")

    monkeypatch.setattr(da, "is_drive_authority_active", lambda *a, **k: True)
    monkeypatch.setattr(da, "emit_operator_initiated", _boom)
    _seed(tmp_graph, [{
        "id": "ab-fr000001",
        "title": "Emit fails",
        "status": "ready",
        "domain": "code",
    }])
    _stub_subprocess_with_stderr(monkeypatch, branch="main", pr_view_rc=0, pr_view_stdout="")
    result = runner.invoke(app, ["done", "ab-fr000001"], catch_exceptions=False)
    assert result.exit_code == 0, result.stdout
    assert _read(tmp_graph)[0]["status"] == "done"
