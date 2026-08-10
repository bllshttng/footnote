"""The plan-closure promise gate (x-5d34).

A merged PR is closing evidence for an ARTIFACT, not for a PLAN. ``resolve_promise_evidence``
adds one condition to the existing close gate - "did the plan's declared work
all ship" - consulted by all three close verbs (``cmd_done``,
``cmd_reconcile``, ``fno done``) so a node can never close through a second,
ungated path.

The gate fires ONLY on an explicit declaration (``close_probes`` or
``expected_url_count``). A plan that declares neither closes exactly as it does
today: inferring "multi-wave" from ``## Wave N`` headings was rejected because
the common case (one .md == one PR == one node) uses waves as internal
structure and would false-positive identically to a half-ship, parking every
such node on autonomous /target. A gate that only fires on an explicit promise
cannot false-positive.

The load-bearing test is the three-verb assertion: a guard landed on only one
of the three close paths is the exact decorative-guard defect this feature
exists to end, so the refusal is exercised against each verb independently.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner


# ---------------------------------------------------------------------------
# plan-doc + graph helpers
# ---------------------------------------------------------------------------


def _write_plan(
    path: Path,
    *,
    waves: int = 0,
    close_probes: list[str] | None = None,
    expected_url_count: int | None = None,
) -> Path:
    """Write a minimal plan doc. `waves` only adds body headings (flavor); the
    gate no longer reads them, so a multi-wave plan declaring nothing closes."""
    lines = ["---", "node: x-prom", "status: ready", "created: 2026-08-09T00:00:00+00:00"]
    if close_probes is not None:
        lines.append("close_probes:")
        for probe in close_probes:
            lines.append(f'  - "{probe}"')
    if expected_url_count is not None:
        lines.append(f"expected_url_count: {expected_url_count}")
    lines.append("---")
    lines.append("")
    lines.append("# Plan")
    lines.append("")
    for n in range(1, waves + 1):
        lines.append(f"## Wave {n}{' -' if n % 2 == 0 else ':'} wave {n} title")
        lines.append("")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


@pytest.fixture
def routed(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Graph + ledger wired into the CLI; returns the graph path."""
    import fno.graph._constants as gc
    import fno.graph.store as gs

    g = tmp_path / "graph.json"
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": []}\n', encoding="utf-8")
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.retro_pending_dir", lambda: tmp_path / "retro")
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n", encoding="utf-8")


def _node(g: Path, node_id: str) -> dict:
    return next(e for e in json.loads(g.read_text())["entries"] if e["id"] == node_id)


def _merged(monkeypatch, *, target="both"):
    """Point both done verbs' gh seams at MERGED."""
    from fno.graph._reconcile import PrMergeState
    import fno.graph.cli as graph_cli
    import fno.done.cli as done_cli

    state = lambda n, **kw: PrMergeState(number=n, state="MERGED", url=None, merged_at="2026-08-09T00:00:00Z")
    if target in ("both", "graph"):
        monkeypatch.setattr(graph_cli, "_done_gh_query", state)
    if target in ("both", "done"):
        monkeypatch.setattr(done_cli, "_gh_query", state)
    return state


def _base_node(node_id: str, plan_path: str) -> dict:
    return {
        "id": node_id,
        "title": f"node {node_id}",
        "domain": "code",
        "status": "in_review",
        "pr_number": 42,
        "pr_url": "https://github.com/o/r/pull/42",
        "plan_path": plan_path,
        "cost_usd": None,
        "cost_sessions": [],
        "created_at": "2026-08-09T00:00:00+00:00",
    }


# ---------------------------------------------------------------------------
# resolve_promise_evidence, direct (no-declaration + conditions B / C + fail-open)
# ---------------------------------------------------------------------------


def test_no_declaration_closes_clean_even_multi_wave(tmp_path: Path):
    """THE design property: a plan declaring neither close_probes nor
    expected_url_count closes exactly as it does today - even with many wave
    headings, since waves are internal structure, not a promise (one .md == one
    PR == one node is the house rule). Inferring from headings would park every
    such node on autonomous /target."""
    from fno.graph._reconcile import resolve_promise_evidence

    plan = _write_plan(tmp_path / "p.md", waves=2)
    assert resolve_promise_evidence({"id": "x-a", "plan_path": str(plan)}).outcome == "ok"


def test_no_plan_path_passes():
    from fno.graph._reconcile import resolve_promise_evidence

    assert resolve_promise_evidence({"id": "x-a", "plan_path": None}).outcome == "ok"
    assert resolve_promise_evidence({"id": "x-a"}).outcome == "ok"


def test_unreadable_plan_fails_open_with_named_warning(tmp_path: Path):
    from fno.graph._reconcile import resolve_promise_evidence

    missing = tmp_path / "nope.md"
    v = resolve_promise_evidence({"id": "x-a", "plan_path": str(missing)})
    assert v.outcome == "ok"
    assert v.warning is not None
    assert str(missing) in v.warning  # the path is named, not just "plan unreadable"


def test_condition_b_passing_probes_pass(tmp_path: Path):
    from fno.graph._reconcile import resolve_promise_evidence

    plan = _write_plan(tmp_path / "p.md", close_probes=["true"])
    v = resolve_promise_evidence(
        {"id": "x-b", "plan_path": str(plan)},
        probe_runner=lambda p, c: (True, ""),
    )
    assert v.outcome == "ok"


def test_condition_b_failing_probe_refuses(tmp_path: Path):
    from fno.graph._reconcile import resolve_promise_evidence

    plan = _write_plan(tmp_path / "p.md", close_probes=["exit 1"])
    v = resolve_promise_evidence(
        {"id": "x-b", "plan_path": str(plan)},
        probe_runner=lambda p, c: (False, "probe `exit 1` exited 1"),
    )
    assert v.outcome == "promise_unmet"
    assert "close_probe" in (v.reason or "")
    assert "exited 1" in (v.reason or "")


# ---------------------------------------------------------------------------
# condition D - unharvested deferred carve-outs
# ---------------------------------------------------------------------------
# The carveout_reader seam returns the post-filter DEFERRED list (the real
# reader delegates the deferred-vs-oos-bug/backfill split to read_carveouts,
# which the carveout-core tests cover). An empty list therefore also stands in
# for "ledger has only oos-bug/backfill" - those are filtered out before the
# gate, which is the load-bearing kind split.


def _cv(cv_id: str, need: str = "a deferred thing") -> dict:
    return {"id": cv_id, "kind": "deferred", "need": need, "description": need}


def test_condition_d_deferred_carveout_refuses(tmp_path: Path):
    from fno.graph._reconcile import resolve_promise_evidence

    plan = _write_plan(tmp_path / "p.md", waves=1)
    v = resolve_promise_evidence(
        {"id": "x-d", "plan_path": str(plan)},
        carveout_reader=lambda cwd: [_cv("cv-99ebc0f3", "item 43 index move")],
    )
    assert v.outcome == "promise_unmet"
    assert v.exit_code == 6
    # The carve-out is named, the scope rationale is stated, and both legal
    # exits (harvest / --force --reason) appear.
    assert "1 unharvested deferred carve-out" in (v.reason or "")
    assert "cv-99ebc0f3" in (v.reason or "")
    assert "item 43 index move" in (v.reason or "")
    assert "--force --reason" in (v.reason or "")
    assert "harvest" in (v.reason or "")


def test_condition_d_empty_ledger_passes(tmp_path: Path):
    from fno.graph._reconcile import resolve_promise_evidence

    plan = _write_plan(tmp_path / "p.md", waves=1)
    v = resolve_promise_evidence(
        {"id": "x-d", "plan_path": str(plan)},
        carveout_reader=lambda cwd: [],
    )
    assert v.outcome == "ok"


def test_condition_d_fires_on_a_multi_wave_plan(tmp_path: Path):
    from fno.graph._reconcile import resolve_promise_evidence

    # Condition D is independent of the plan: a multi-wave plan that declared no
    # assertion closes cleanly on its own (wave-inference was rejected), but a
    # deferred carve-out still holds it. D fires and names the carve-out, never
    # a wave count.
    plan = _write_plan(tmp_path / "p.md", waves=2)
    v = resolve_promise_evidence(
        {"id": "x-d", "plan_path": str(plan)},
        carveout_reader=lambda cwd: [_cv("cv-deadbeef")],
    )
    assert v.outcome == "promise_unmet"
    assert "deferred carve-out" in (v.reason or "")
    assert "promised 2 waves" not in (v.reason or "")


def test_carveout_ledger_root_resolves_from_the_nodes_project(tmp_path):
    """The ledger root is the canonical of the repo at ``cwd``, not the ambient
    command repo, so a cross-project close reads the foreign project's ledger
    rather than the session's (codex P1: the resolver must follow the node)."""
    import subprocess
    from fno.graph._reconcile import _carveout_ledger_root

    repo = tmp_path / "foreign"
    repo.mkdir()
    subprocess.run(["git", "init", "-q"], cwd=str(repo), check=True)
    root = _carveout_ledger_root(str(repo))
    assert Path(root).resolve() == repo.resolve()


def test_condition_b_fail_closed_when_binary_absent(tmp_path: Path, monkeypatch):
    from fno.graph._reconcile import resolve_promise_evidence
    import fno.rust_binary as rb

    plan = _write_plan(tmp_path / "p.md", close_probes=["true"])
    monkeypatch.setattr(rb, "resolve_binary", lambda: None)
    v = resolve_promise_evidence({"id": "x-b", "plan_path": str(plan)})
    assert v.outcome == "promise_unmet"
    assert "fno-agents binary was not found" in (v.reason or "")


def test_condition_c_shortfall_refuses(tmp_path: Path):
    from fno.graph._reconcile import resolve_promise_evidence, PrMergeState

    plan = _write_plan(tmp_path / "p.md", expected_url_count=3)
    node = {
        "id": "x-c",
        "plan_path": str(plan),
        "pr_number": 42,
        "pr_url": "https://github.com/o/r/pull/42",
    }
    merged = lambda n, **kw: PrMergeState(number=n, state="MERGED", url=None, merged_at=None)
    v = resolve_promise_evidence(node, query=merged)
    assert v.outcome == "promise_unmet"
    assert "promised 3 ships" in (v.reason or "")
    assert "only 1 merged" in (v.reason or "")


def test_condition_c_satisfied_passes(tmp_path: Path):
    from fno.graph._reconcile import resolve_promise_evidence, PrMergeState

    plan = _write_plan(tmp_path / "p.md", expected_url_count=3)
    node = {
        "id": "x-c",
        "plan_path": str(plan),
        "pr_number": 1,
        "pr_url": "https://github.com/o/r/pull/1",
        "additional_prs": [
            {"number": 2, "url": "https://github.com/o/r/pull/2"},
            {"number": 3, "url": "https://github.com/o/r/pull/3"},
        ],
    }
    merged = lambda n, **kw: PrMergeState(number=n, state="MERGED", url=None, merged_at=None)
    assert resolve_promise_evidence(node, query=merged).outcome == "ok"


def test_condition_c_gh_outage_fails_open_not_refuses(tmp_path: Path):
    """An unreachable ref is not a missing ship. Counting a gh outage as
    unmerged told the operator the plan under-shipped when gh was simply down;
    the merge gate treats the same outage as retryable, not a policy refusal."""
    from fno.graph._reconcile import ReconcileError, resolve_promise_evidence

    plan = _write_plan(tmp_path / "p.md", expected_url_count=2)
    node = {
        "id": "x-c",
        "plan_path": str(plan),
        "pr_number": 1,
        "pr_url": "https://github.com/o/r/pull/1",
        "additional_prs": [{"number": 2, "url": "https://github.com/o/r/pull/2"}],
    }

    def _down(n, **kw):
        raise ReconcileError("gh pr view timed out")

    v = resolve_promise_evidence(node, query=_down)
    assert v.outcome == "ok"
    assert "could not confirm 2 ships" in (v.warning or "")


def test_promise_verdict_exit_code_table():
    from fno.graph._reconcile import PromiseVerdict

    assert PromiseVerdict(outcome="ok").exit_code == 0
    assert PromiseVerdict(outcome="promise_unmet").exit_code == 6


def test_relative_plan_resolved_against_owning_cwd(tmp_path: Path):
    """A repo-relative plan_path is read from the node's cwd, not the process
    CWD - the global reconcile runs from canonical while a node's plan lives in
    its owning checkout. Uses a DECLARATION so a fail-open (plan not found)
    would read as ok, not promise_unmet: condition C firing proves the plan was
    read from cwd (P1)."""
    from fno.graph._reconcile import resolve_promise_evidence, PrMergeState

    repo = tmp_path / "repo"
    repo.mkdir()
    _write_plan(repo / "rel.md", expected_url_count=2)
    node = {"id": "x-rel", "plan_path": "rel.md", "pr_number": 7,
            "pr_url": "https://github.com/o/r/pull/7"}
    merged = lambda n, **kw: PrMergeState(number=n, state="MERGED", url=None, merged_at=None)
    v = resolve_promise_evidence(node, cwd=str(repo), query=merged)
    assert v.outcome == "promise_unmet"  # found the declaration via cwd, did not fail open


def test_condition_c_counts_extra_refs(tmp_path: Path):
    """An explicit ship the close verb is recording (extra_refs, e.g. the new
    --pr on `fno done`) counts toward expected_url_count, so a final multi-PR
    close is not deadlocked by its own new PR (P2)."""
    from fno.graph._reconcile import resolve_promise_evidence, PrMergeState

    plan = _write_plan(tmp_path / "p.md", expected_url_count=2)
    node = {
        "id": "x-c",
        "plan_path": str(plan),
        "pr_number": 1,
        "pr_url": "https://github.com/o/r/pull/1",
    }
    merged = lambda n, **kw: PrMergeState(number=n, state="MERGED", url=None, merged_at=None)
    # Stored ref #1 merged + explicit #2 (extra_refs) merged -> 2 >= 2 -> ok.
    v = resolve_promise_evidence(
        node, query=merged, extra_refs=[(2, "https://github.com/o/r/pull/2")]
    )
    assert v.outcome == "ok"
    # Without the extra ref: only 1 merged < 2 -> refuse.
    assert resolve_promise_evidence(node, query=merged).outcome == "promise_unmet"


# ---------------------------------------------------------------------------
# THE THREE-VERB ASSERTION: a declared shortfall refuses on each close path
# ---------------------------------------------------------------------------


def _shortfall_world(g: Path, tmp_path: Path, node_id: str = "ab-prom01") -> str:
    """A node whose plan DECLARES expected_url_count=2 but has one PR ref.
    The merge gate passes (the one PR is merged); the promise gate refuses."""
    plan = _write_plan(tmp_path / "shortfall.md", expected_url_count=2)
    _seed(g, [_base_node(node_id, str(plan))])
    return str(plan)


def test_condition_C_refuses_on_cmd_done(routed, tmp_path, monkeypatch):
    _shortfall_world(routed, tmp_path)
    _merged(monkeypatch, target="graph")
    from fno.cli import app

    r = CliRunner().invoke(app, ["backlog", "done", "ab-prom01"])
    assert r.exit_code == 6, r.output
    assert _node(routed, "ab-prom01").get("completed_at") is None


def test_condition_C_refuses_on_fno_done(routed, tmp_path, monkeypatch):
    _shortfall_world(routed, tmp_path)
    _merged(monkeypatch, target="done")
    from fno.cli import app

    r = CliRunner().invoke(app, ["done", "ab-prom01", "--pr", "42"])
    assert r.exit_code == 6, r.output
    assert _node(routed, "ab-prom01").get("completed_at") is None


def test_condition_C_holds_open_on_reconcile(routed, tmp_path, monkeypatch):
    plan = _shortfall_world(routed, tmp_path)
    import fno.graph._reconcile as rec
    from fno.graph._reconcile import PrMergeState

    # Reconcile's resolve_promise_evidence uses the default query_pr_merge_state
    # (not the verb shims), so stub it here too - condition C re-counts merges.
    monkeypatch.setattr(
        rec, "query_pr_merge_state",
        lambda n, **kw: PrMergeState(number=n, state="MERGED", url=None, merged_at=None),
    )

    def _scan(entries, node_id=None):
        return [rec.MergeDriftRecord(
            node_id="ab-prom01",
            plan_path=plan,
            pr_number=42,
            pr_url="https://github.com/o/r/pull/42",
            pr_state="MERGED",
            merged_at="2026-08-09T00:00:00Z",
        )]

    monkeypatch.setattr(rec, "scan_merge_drift", _scan)
    from fno.graph.cli import cli

    r = CliRunner().invoke(cli, ["reconcile", "--json"])
    payload = json.loads(r.output)
    # The node is held open, not closed, and named in the sweep summary.
    assert any(p["node_id"] == "ab-prom01" for p in payload["promise_unmet"])
    assert all(c.get("node_id") != "ab-prom01" for c in payload["closed"])
    assert _node(routed, "ab-prom01").get("completed_at") is None


def test_undeclared_plan_closes_on_all_three_verbs(routed, tmp_path, monkeypatch):
    """The inverse of the refusal: a plan declaring nothing closes clean on
    every close path - a regression on any single path that re-introduces an
    ungated writer is caught because the other two still close."""
    plan = _write_plan(tmp_path / "plain.md")  # no declaration
    _seed(routed, [
        _base_node("ab-d1", str(plan)),
        _base_node("ab-d2", str(plan)),
        _base_node("ab-d3", str(plan)),
    ])
    _merged(monkeypatch)
    from fno.cli import app

    assert CliRunner().invoke(app, ["backlog", "done", "ab-d1"]).exit_code == 0
    assert CliRunner().invoke(app, ["done", "ab-d2", "--pr", "42"]).exit_code == 0
    # reconcile: stub the scan to mark x-d3 closeable.
    import fno.graph._reconcile as rec

    def _scan(entries, node_id=None):
        return [rec.MergeDriftRecord(
            node_id="ab-d3", plan_path=str(plan), pr_number=42,
            pr_url="https://github.com/o/r/pull/42", pr_state="MERGED",
            merged_at="2026-08-09T00:00:00Z",
        )]

    monkeypatch.setattr(rec, "scan_merge_drift", _scan)
    from fno.graph.cli import cli

    assert CliRunner().invoke(cli, ["reconcile", "--json"]).exit_code == 0
    assert _node(routed, "ab-d1").get("completed_at") is not None
    assert _node(routed, "ab-d2").get("completed_at") is not None
    assert _node(routed, "ab-d3").get("completed_at") is not None


# ---------------------------------------------------------------------------
# condition D through the verbs (carve-out ledger) + --force bypass
# ---------------------------------------------------------------------------


def test_condition_D_refuses_on_all_three_verbs(routed, tmp_path, monkeypatch):
    """A deferred carve-out holds the node open on every close path - the
    one-of-N decorative-guard shape this feature exists to end (a gate on a
    single verb would let a second path close around it)."""
    import fno.graph._reconcile as rec

    plan = _write_plan(tmp_path / "one-wave.md", waves=1)
    _seed(routed, [
        _base_node("ab-dc1", str(plan)),
        _base_node("ab-dc2", str(plan)),
        _base_node("ab-dc3", str(plan)),
    ])
    monkeypatch.setattr(
        rec,
        "_unharvested_deferred_carveouts",
        lambda cwd: [{"id": "cv-99ebc0f3", "kind": "deferred", "need": "item 43"}],
    )
    _merged(monkeypatch)
    from fno.cli import app

    assert CliRunner().invoke(app, ["backlog", "done", "ab-dc1"]).exit_code == 6
    assert _node(routed, "ab-dc1").get("completed_at") is None

    assert CliRunner().invoke(app, ["done", "ab-dc2", "--pr", "42"]).exit_code == 6
    assert _node(routed, "ab-dc2").get("completed_at") is None

    def _scan(entries, node_id=None):
        return [rec.MergeDriftRecord(
            node_id="ab-dc3", plan_path=str(plan), pr_number=42,
            pr_url="https://github.com/o/r/pull/42", pr_state="MERGED",
            merged_at="2026-08-09T00:00:00Z",
        )]

    monkeypatch.setattr(rec, "scan_merge_drift", _scan)
    from fno.graph.cli import cli

    payload = json.loads(CliRunner().invoke(cli, ["reconcile", "--json"]).output)
    assert any(p["node_id"] == "ab-dc3" for p in payload["promise_unmet"])
    assert all(c.get("node_id") != "ab-dc3" for c in payload["closed"])
    assert _node(routed, "ab-dc3").get("completed_at") is None


def test_condition_D_force_bypass_closes(routed, tmp_path, monkeypatch):
    """--force --reason records the carve-out as accepted and closes; the
    promise gate never runs on the force path, so a deliberate deferred-scope
    close is a journal line, not a hard wedge."""
    import fno.graph._reconcile as rec

    plan = _write_plan(tmp_path / "one-wave.md", waves=1)
    _seed(routed, [_base_node("ab-dc4", str(plan))])
    monkeypatch.setattr(
        rec,
        "_unharvested_deferred_carveouts",
        lambda cwd: [{"id": "cv-99ebc0f3", "kind": "deferred", "need": "item 43"}],
    )
    _merged(monkeypatch, target="graph")
    from fno.cli import app

    r = CliRunner().invoke(
        app,
        ["backlog", "done", "ab-dc4", "--force", "--reason", "cv-99ebc0f3 filed as x-rest"],
    )
    assert r.exit_code == 0, r.output
    assert _node(routed, "ab-dc4").get("completed_at") is not None


# ---------------------------------------------------------------------------
# --force bypass + ship count (condition C) through the verbs
# ---------------------------------------------------------------------------


def test_force_bypass_closes_a_ship_shortfall(routed, tmp_path, monkeypatch):
    """A deliberate half-ship is a recorded line, not silence. `--force --reason`
    journals and closes; the promise gate never runs on the force path."""
    _shortfall_world(routed, tmp_path)
    _merged(monkeypatch, target="graph")
    from fno.cli import app

    r = CliRunner().invoke(
        app, ["backlog", "done", "ab-prom01", "--force", "--reason", "second ship filed as ab-rest"]
    )
    assert r.exit_code == 0, r.output
    assert _node(routed, "ab-prom01").get("completed_at") is not None


def test_condition_c_through_cmd_done_refuses_then_closes(routed, tmp_path, monkeypatch):
    plan = _write_plan(tmp_path / "c.md", expected_url_count=3)
    _seed(routed, [_base_node("ab-ship1", str(plan))])
    _merged(monkeypatch, target="graph")
    from fno.cli import app

    # One merged ref < 3 promised -> refused.
    assert CliRunner().invoke(app, ["backlog", "done", "ab-ship1"]).exit_code == 6
    assert _node(routed, "ab-ship1").get("completed_at") is None

    # Three merged refs satisfy the promise -> closes.
    _seed(routed, [{
        **_base_node("ab-ship3", str(plan)),
        "additional_prs": [
            {"number": 43, "url": "https://github.com/o/r/pull/43"},
            {"number": 44, "url": "https://github.com/o/r/pull/44"},
        ],
    }])
    assert CliRunner().invoke(app, ["backlog", "done", "ab-ship3"]).exit_code == 0
    assert _node(routed, "ab-ship3").get("completed_at") is not None
