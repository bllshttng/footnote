"""Regression guards for the done=merged invariant (x-47a3).

A backlog node may only close on MERGED evidence. Historically the finalize
ledger append shelled an ungated ``update --completed`` leg and closed nodes
at ship time, 2h before their PR merged. These tests pin the writers shut.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest


# ---------------------------------------------------------------------------
# AC1-HP: a ledger append never closes a graph node
# ---------------------------------------------------------------------------


def _seed_graph(home: Path, *, plan_path: str) -> Path:
    """Write a graph fixture whose node is the ledger entry's join target."""
    graph = home / ".fno" / "graph.json"
    graph.parent.mkdir(parents=True, exist_ok=True)
    graph.write_text(
        json.dumps(
            {
                "entries": [
                    {
                        "id": "x-fixture",
                        "plan_path": plan_path,
                        "pr_number": 505,
                        "_status": "in_review",
                    }
                ]
            },
            indent=2,
        ),
        encoding="utf-8",
    )
    return graph


def test_register_entry_leaves_graph_byte_identical(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A finalize ledger append must not write to graph.json at all.

    Two assertions, because either alone is weak. Byte-identity catches an
    in-process writer but is only as red as the subprocess it fails to spawn;
    the shell-out assertion catches the actual incident shape (the deleted leg
    shelled ``roadmap-tasks.py update --completed``) deterministically, without
    depending on whether that subprocess would have succeeded here.

    The deleted writer fired only when the entry carried plan_path AND
    pr_number, so the entry below is exactly its trigger shape.
    """
    from fno.cost import _register

    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))

    plan_path = str(tmp_path / "plan.md")
    graph = _seed_graph(tmp_path, plan_path=plan_path)
    before = graph.read_bytes()

    ledger = tmp_path / "ledger.json"
    monkeypatch.setattr(_register._paths, "ledger_json", lambda: ledger)

    shelled: list[list[str]] = []
    real_run = _register.subprocess.run

    def _spy(cmd, *a, **kw):
        shelled.append([str(c) for c in cmd] if isinstance(cmd, (list, tuple)) else [str(cmd)])
        return real_run(cmd, *a, **kw)

    monkeypatch.setattr(_register.subprocess, "run", _spy)

    _register.register_entry(
        {
            "type": "execution",
            "plan_path": plan_path,
            "pr_number": 505,
            "pr_url": "https://github.com/o/r/pull/505",
            "graph_node_id": "x-e4bc",
            "session_id": "sess-1",
            "root_path": str(tmp_path),
        }
    )

    assert ledger.exists(), "the ledger append itself must still land"
    assert graph.read_bytes() == before, "ledger append must not mutate graph.json"

    graph_writes = [
        c for c in shelled if any("roadmap-tasks" in p or "--completed" in p for p in c)
    ]
    assert not graph_writes, f"ledger append shelled a graph close: {graph_writes}"


def test_register_module_has_no_graph_sync_leg() -> None:
    """The sync helpers are deleted, not merely unreferenced."""
    from fno.cost import _register

    for name in ("_sync_to_graph", "_match_graph_node", "_normalize_plan_path"):
        assert not hasattr(_register, name), f"{name} must stay deleted"


# ---------------------------------------------------------------------------
# AC2-HP / AC3-ERR / AC5-EDGE: `fno done` gates like `backlog done`
# ---------------------------------------------------------------------------


@pytest.fixture
def done_graph(tmp_path, monkeypatch) -> Path:
    """A graph routed to the done command's code path, with an empty ledger."""
    g = tmp_path / "graph.json"
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    return g


def _seed(g: Path, entry: dict) -> None:
    g.write_text(json.dumps({"entries": [entry]}, indent=2) + "\n")


def _node(g: Path, node_id: str) -> dict:
    return next(e for e in json.loads(g.read_text())["entries"] if e["id"] == node_id)


def _stub_gh(monkeypatch, state: str | None, *, calls: list | None = None):
    """Point `fno done`'s gh seam at a fixed state, or raise for an outage."""
    import fno.done.cli as done_cli
    from fno.graph._reconcile import PrMergeState, ReconcileError

    def _q(pr_number, **kwargs):
        if calls is not None:
            calls.append(pr_number)
        if state is None:
            raise ReconcileError("gh: not authenticated")
        return PrMergeState(
            number=pr_number, state=state, url=None,
            merged_at="2026-01-01T00:00:00Z" if state == "MERGED" else None,
        )

    monkeypatch.setattr(done_cli, "_gh_query", _q)


def test_ac2_hp_done_on_open_pr_exits_5_after_querying_gh(done_graph, monkeypatch):
    """AC2-HP: an OPEN PR is awaiting merge, not a close - and gh is consulted.

    The call is asserted, not just the exit code: an implementation that
    exits 5 without querying would pass a code-only check while being blind
    to the actual PR state.
    """
    from typer.testing import CliRunner
    from fno.cli import app

    _seed(done_graph, {"id": "ab-open001", "title": "Open PR node", "domain": "code"})
    calls: list = []
    _stub_gh(monkeypatch, "OPEN", calls=calls)

    r = CliRunner().invoke(app, ["done", "ab-open001", "--pr", "42"])

    assert r.exit_code == 5
    assert calls == [42], "the gate must query gh, not assume"

    entry = _node(done_graph, "ab-open001")
    assert entry.get("completed_at") is None
    assert entry.get("_status") != "done"
    assert entry.get("merge_status") is None


def test_ac3_err_gh_failure_fails_closed(done_graph, monkeypatch):
    """AC3-ERR: an unreachable gh refuses the close rather than trusting --pr."""
    from typer.testing import CliRunner
    from fno.cli import app

    _seed(done_graph, {"id": "ab-out001", "title": "Outage node", "domain": "code"})
    _stub_gh(monkeypatch, None)

    r = CliRunner().invoke(app, ["done", "ab-out001", "--pr", "42"])

    assert r.exit_code == 4
    entry = _node(done_graph, "ab-out001")
    assert entry.get("completed_at") is None
    assert entry.get("merge_status") is None


def test_merged_pr_closes_and_records_resolved_merge_status(done_graph, monkeypatch):
    """merge_status is written from the gh-resolved state, never a literal."""
    from typer.testing import CliRunner
    from fno.cli import app

    _seed(done_graph, {"id": "ab-mrg001", "title": "Merged node", "domain": "code"})
    _stub_gh(monkeypatch, "MERGED")

    r = CliRunner().invoke(app, ["done", "ab-mrg001", "--pr", "42"])

    assert r.exit_code == 0
    entry = _node(done_graph, "ab-mrg001")
    assert entry.get("completed_at") is not None
    assert entry.get("merge_status") == "merged"


def test_ac5_edge_non_pr_close_is_ungated(done_graph, monkeypatch):
    """AC5-EDGE: a node closing on --note has no PR, so the gate never applies."""
    from typer.testing import CliRunner
    from fno.cli import app

    _seed(done_graph, {"id": "ab-doc001", "title": "Docs node", "domain": "docs"})
    calls: list = []
    _stub_gh(monkeypatch, None, calls=calls)  # would raise if consulted

    r = CliRunner().invoke(app, ["done", "ab-doc001", "--note", "shipped brief"])

    assert r.exit_code == 0
    assert calls == [], "a PR-less close must not query gh"
    entry = _node(done_graph, "ab-doc001")
    assert entry.get("completed_at") is not None
    assert entry.get("completion_note") == "shipped brief"


# ---------------------------------------------------------------------------
# US5 / AC1-HP + AC4-FR: sweep every inventoried writer against one fixture
# ---------------------------------------------------------------------------


def test_us5_no_writer_closes_a_node_whose_pr_is_open(done_graph, monkeypatch):
    """Every close path refuses an OPEN PR, so none can regress alone.

    The inventory that produced this fix listed six writers; W1/W2 were already
    merge-gated, W5 is deleted, and W3/W4 are fixed here. Sweeping them against
    one fixture is what keeps a future seventh writer from quietly rejoining the
    list - the node must read in_review afterward no matter who tried.
    """
    from typer.testing import CliRunner
    from fno.cli import app
    from fno.graph._reconcile import PrMergeState
    import fno.graph.cli as graph_cli

    _seed(done_graph, {
        "id": "ab-sweep01",
        "title": "Open PR sweep node",
        "domain": "code",
        "_status": "in_review",
        "pr_number": 42,
        "pr_url": "https://github.com/o/r/pull/42",
    })
    _stub_gh(monkeypatch, "OPEN")
    monkeypatch.setattr(
        graph_cli,
        "_done_gh_query",
        lambda n, **kw: PrMergeState(number=n, state="OPEN", url=None, merged_at=None),
    )

    runner = CliRunner()
    # W4 `fno done`, W1 `backlog done`: both must report awaiting merge.
    assert runner.invoke(app, ["done", "ab-sweep01", "--pr", "42"]).exit_code == 5
    assert runner.invoke(app, ["backlog", "done", "ab-sweep01"]).exit_code == 5
    # W3 `backlog update --completed`: the flag is gone, so it cannot be reached.
    assert runner.invoke(
        app, ["backlog", "update", "ab-sweep01", "--completed"]
    ).exit_code != 0

    entry = _node(done_graph, "ab-sweep01")
    assert entry.get("completed_at") is None
    assert entry.get("_status") == "in_review"
    assert entry.get("merge_status") is None


def test_ac4_fr_completed_at_never_precedes_merged_at(done_graph, monkeypatch):
    """AC4-FR: the invariant the incident violated by 2h05m."""
    from datetime import datetime
    from typer.testing import CliRunner
    from fno.cli import app

    merged_at = "2026-01-01T00:00:00+00:00"
    _seed(done_graph, {"id": "ab-mrg002", "title": "Merged node", "domain": "code"})
    _stub_gh(monkeypatch, "MERGED")

    r = CliRunner().invoke(app, ["done", "ab-mrg002", "--pr", "42"])
    assert r.exit_code == 0

    entry = _node(done_graph, "ab-mrg002")
    assert datetime.fromisoformat(entry["completed_at"]) >= datetime.fromisoformat(merged_at)
