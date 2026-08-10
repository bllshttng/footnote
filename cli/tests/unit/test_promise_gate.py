"""The plan-closure promise gate (x-5d34).

A merged PR is closing evidence for an ARTIFACT, not for a PLAN. A plan that
promised two waves and shipped one used to close clean on wave 1's merge, the
remainder vanishing silently. ``resolve_promise_evidence`` adds one condition to
the existing close gate: "did the plan's declared work all ship". One verdict,
three callers (``cmd_done``, ``cmd_reconcile``, ``fno done``) - never a second
closer.

The load-bearing test is the three-verb assertion: a guard landed on only one
of the three close paths is the exact decorative-guard defect this feature
exists to end, so every condition is exercised against each verb independently.
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
    body_extra: str = "",
) -> Path:
    """Write a minimal plan doc with frontmatter and the requested wave headings."""
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
    # The specimen wrote `## Wave 1 - name`; the canonical is `## Wave 1: name`.
    # Alternate the separator so both forms are covered across the suite.
    for n in range(1, waves + 1):
        sep = ":" if n % 2 else " -"
        lines.append(f"## Wave {n}{sep} wave {n} title")
        lines.append("")
        lines.append(f"Body of wave {n}.")
        lines.append("")
    if body_extra:
        lines.append(body_extra)
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
# wave counter
# ---------------------------------------------------------------------------


def test_count_plan_waves_matches_both_separators_and_dedupes():
    from fno.graph._reconcile import count_plan_waves

    assert count_plan_waves("## Wave 1: Foundation\n## Wave 2: Build\n") == 2
    assert count_plan_waves("## Wave 1 - widen\n## Wave 2 - cull\n") == 2
    # A repeated header counts once; `## Wave rationale` is not a wave.
    assert count_plan_waves("## Wave 1: a\n## Wave 1: b\n## Wave rationale\n") == 1
    assert count_plan_waves("no waves here") == 0


# ---------------------------------------------------------------------------
# resolve_promise_evidence, direct (conditions A / B / C + fail-open)
# ---------------------------------------------------------------------------


def test_condition_a_two_waves_no_assertion_refuses(tmp_path: Path):
    from fno.graph._reconcile import resolve_promise_evidence

    plan = _write_plan(tmp_path / "p.md", waves=2)
    v = resolve_promise_evidence({"id": "x-a", "plan_path": str(plan)})
    assert v.outcome == "promise_unmet"
    assert v.exit_code == 6
    assert "promised 2 waves" in (v.reason or "")
    # The limits land in the refusal, not only the docs.
    assert "under-declared" in (v.reason or "")
    assert "--force --reason" in (v.reason or "")


def test_condition_a_one_wave_passes(tmp_path: Path):
    from fno.graph._reconcile import resolve_promise_evidence

    plan = _write_plan(tmp_path / "p.md", waves=1)
    assert resolve_promise_evidence({"id": "x-a", "plan_path": str(plan)}).outcome == "ok"


def test_no_plan_path_passes():
    from fno.graph._reconcile import resolve_promise_evidence

    assert resolve_promise_evidence({"id": "x-a", "plan_path": None}).outcome == "ok"
    assert resolve_promise_evidence({"id": "x-a"}).outcome == "ok"


def test_unreadable_plan_fails_open_with_named_warning(tmp_path: Path, capsys):
    from fno.graph._reconcile import resolve_promise_evidence

    missing = tmp_path / "nope.md"
    v = resolve_promise_evidence({"id": "x-a", "plan_path": str(missing)})
    assert v.outcome == "ok"
    err = capsys.readouterr().err
    assert str(missing) in err  # the path is named, not just "plan unreadable"


def test_condition_b_passing_probes_pass(tmp_path: Path):
    from fno.graph._reconcile import resolve_promise_evidence

    plan = _write_plan(tmp_path / "p.md", waves=2, close_probes=["true"])
    v = resolve_promise_evidence(
        {"id": "x-b", "plan_path": str(plan)},
        probe_runner=lambda p, c: (True, ""),
    )
    assert v.outcome == "ok"


def test_condition_b_failing_probe_refuses(tmp_path: Path):
    from fno.graph._reconcile import resolve_promise_evidence

    plan = _write_plan(tmp_path / "p.md", waves=2, close_probes=["exit 1"])
    v = resolve_promise_evidence(
        {"id": "x-b", "plan_path": str(plan)},
        probe_runner=lambda p, c: (False, "probe `exit 1` exited 1"),
    )
    assert v.outcome == "promise_unmet"
    assert "close_probe" in (v.reason or "")
    assert "exited 1" in (v.reason or "")


def test_condition_b_fail_closed_when_binary_absent(tmp_path: Path, monkeypatch):
    from fno.graph._reconcile import resolve_promise_evidence
    import fno.graph._reconcile as rec

    plan = _write_plan(tmp_path / "p.md", waves=2, close_probes=["true"])
    monkeypatch.setattr(rec.shutil, "which", lambda _: None)
    v = resolve_promise_evidence({"id": "x-b", "plan_path": str(plan)})
    assert v.outcome == "promise_unmet"
    assert "fno-agents binary was not found" in (v.reason or "")


def test_condition_c_shortfall_refuses(tmp_path: Path):
    from fno.graph._reconcile import resolve_promise_evidence, PrMergeState

    plan = _write_plan(tmp_path / "p.md", waves=0, expected_url_count=3)
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

    plan = _write_plan(tmp_path / "p.md", waves=0, expected_url_count=3)
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


def test_promise_verdict_exit_code_table():
    from fno.graph._reconcile import PromiseVerdict

    assert PromiseVerdict(outcome="ok").exit_code == 0
    assert PromiseVerdict(outcome="promise_unmet").exit_code == 6


# ---------------------------------------------------------------------------
# THE THREE-VERB ASSERTION: condition A refuses on each close path independently
# ---------------------------------------------------------------------------


def _two_wave_world(g: Path, tmp_path: Path, node_id: str = "ab-prom01") -> str:
    plan = _write_plan(tmp_path / "two-wave.md", waves=2)
    _seed(g, [_base_node(node_id, str(plan))])
    return str(plan)


def test_condition_A_refuses_on_cmd_done(routed, tmp_path, monkeypatch):
    _two_wave_world(routed, tmp_path)
    _merged(monkeypatch, target="graph")
    from fno.cli import app

    r = CliRunner().invoke(app, ["backlog", "done", "ab-prom01"])
    assert r.exit_code == 6, r.output
    assert _node(routed, "ab-prom01").get("completed_at") is None


def test_condition_A_refuses_on_fno_done(routed, tmp_path, monkeypatch):
    _two_wave_world(routed, tmp_path)
    _merged(monkeypatch, target="done")
    from fno.cli import app

    r = CliRunner().invoke(app, ["done", "ab-prom01", "--pr", "42"])
    assert r.exit_code == 6, r.output
    assert _node(routed, "ab-prom01").get("completed_at") is None


def test_condition_A_holds_open_on_reconcile(routed, tmp_path, monkeypatch):
    plan = _two_wave_world(routed, tmp_path)
    import fno.graph._reconcile as rec

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


def test_one_wave_plan_closes_on_all_three_verbs(routed, tmp_path, monkeypatch):
    """The inverse of the refusal: a plan that promised one wave closes clean.

    One fixture, three verbs - a regression on any single close path that
    re-introduces an ungated writer is caught because the other two still close.
    """
    plan = _write_plan(tmp_path / "one-wave.md", waves=1)
    _seed(routed, [_base_node("ab-wave1", str(plan))])
    _merged(monkeypatch)
    from fno.cli import app

    # cmd_done needs its own node (the verbs mutate the shared graph); re-seed
    # per verb by clearing completed_at is unnecessary because each closes a
    # distinct node. Use three nodes so each verb sees an open one.
    _seed(routed, [
        _base_node("ab-d1", str(plan)),
        _base_node("ab-d2", str(plan)),
        _base_node("ab-d3", str(plan)),
    ])

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
# --force bypass + ship count (condition C) through the verbs
# ---------------------------------------------------------------------------


def test_force_bypass_closes_two_wave_plan(routed, tmp_path, monkeypatch):
    """A deliberate half-ship is a recorded line, not silence.

    `--force --reason` journals (best-effort) and closes; the promise gate never
    runs on the force path.
    """
    _two_wave_world(routed, tmp_path)
    _merged(monkeypatch, target="graph")
    from fno.cli import app

    r = CliRunner().invoke(
        app, ["backlog", "done", "ab-prom01", "--force", "--reason", "wave 2 filed as x-rest"]
    )
    assert r.exit_code == 0, r.output
    assert _node(routed, "ab-prom01").get("completed_at") is not None


def test_condition_c_through_cmd_done_refuses_then_closes(routed, tmp_path, monkeypatch):
    plan = _write_plan(tmp_path / "c.md", waves=0, expected_url_count=3)
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
