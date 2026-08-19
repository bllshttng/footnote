from __future__ import annotations

import json

from typer.testing import CliRunner

from fno.cli import app
from fno.graph.ladder import DispatchHoldState
from fno.pr import _hold
from fno.pr.closure import PrClosureContext


def _graph(tmp_path, monkeypatch, *, plan_body: str, pr_body: str = "", entries=None):
    plan = tmp_path / "held.md"
    plan.write_text(plan_body)
    graph = tmp_path / "graph.json"
    graph.write_text(json.dumps({"entries": entries or [{
        "id": "x-5a5c",
        "cwd": str(tmp_path),
        "pr_number": 42,
        "pr_url": "https://github.com/o/r/pull/42",
        "plan_path": str(plan),
    }]}))
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr(
        "fno.pr.closure.fetch_pr_closure_context",
        lambda pr_number, **k: PrClosureContext(
            number=pr_number, body=pr_body, url="https://github.com/o/r/pull/42",
            state="MERGED", merged_at="2026-01-01T00:00:00Z",
        ),
    )
    return graph


def test_hold_for_pr_returns_attributable_plan_hold(tmp_path, monkeypatch):
    _graph(
        tmp_path,
        monkeypatch,
        plan_body=(
            "---\nstatus: ready\ndispatch_hold:\n"
            "  reason: Blocking finding\n  release_when: Finding fixed\n"
            "  review_on: 2099-08-20\n  set_by: king:119e3c52\n---\n"
        ),
    )
    verdict = _hold.hold_for_pr(42, str(tmp_path))
    assert verdict is not None
    assert verdict.owner_id == "x-5a5c"
    assert verdict.hold.state is DispatchHoldState.HELD
    assert verdict.hold.set_by == "king:119e3c52"


def test_hold_for_pr_fails_closed_when_bound_plan_is_unreadable(tmp_path, monkeypatch):
    graph = _graph(tmp_path, monkeypatch, plan_body="---\nstatus: ready\n---\n")
    data = json.loads(graph.read_text())
    malformed = tmp_path / "malformed.md"
    malformed.write_text("---\nstatus: ready\ndispatch_hold: [\n")
    data["entries"][0]["plan_path"] = str(malformed)
    graph.write_text(json.dumps(data))
    reason = _hold.merge_hold_reason(42, str(tmp_path))
    assert reason is not None and "dispatch-hold-invalid:x-5a5c" in reason


def test_hold_for_pr_catches_a_held_node_named_only_on_the_trailer(tmp_path, monkeypatch):
    """Round-10 review fix: a node with no pr_number/pr_url ref of its own -
    named only on the PR's Backlog-Closure trailer - must still be
    hold-checked before merge, since bind_closure_claims closes it post-merge
    with no hold check of its own."""
    unheld_plan = tmp_path / "unheld.md"
    unheld_plan.write_text("---\nstatus: ready\n---\n")
    held_plan = tmp_path / "trailer-held.md"
    held_plan.write_text(
        "---\nstatus: ready\ndispatch_hold:\n"
        "  reason: Blocking finding\n  release_when: Finding fixed\n"
        "  review_on: 2099-08-20\n  set_by: king:119e3c52\n---\n"
    )
    _graph(
        tmp_path,
        monkeypatch,
        plan_body="---\nstatus: ready\n---\n",  # unused: entries is given explicitly below
        pr_body="Backlog-Closure: x-5a5c x-1111",
        entries=[
            {
                "id": "x-5a5c", "cwd": str(tmp_path), "pr_number": 42,
                "pr_url": "https://github.com/o/r/pull/42",
                "plan_path": str(unheld_plan),
            },
            {"id": "x-1111", "cwd": str(tmp_path), "plan_path": str(held_plan)},
        ],
    )
    verdict = _hold.hold_for_pr(42, str(tmp_path))
    assert verdict is not None
    assert verdict.owner_id == "x-1111"
    assert verdict.hold.state is DispatchHoldState.HELD


def test_hold_for_pr_trailer_only_claim_with_no_ref_stamp_is_still_checked(tmp_path, monkeypatch):
    """Even when NOTHING in the graph carries this PR's ref (a brand-new
    node never stamped at all), a trailer claim on an unheld node returns
    None, not a spurious hold - the trailer path is additive, not a
    fail-closed trap on every unstamped merge."""
    plan = tmp_path / "unheld.md"
    plan.write_text("---\nstatus: ready\n---\n")
    _graph(
        tmp_path,
        monkeypatch,
        plan_body="---\nstatus: ready\n---\n",
        pr_body="Backlog-Closure: x-1111",
        entries=[{"id": "x-1111", "cwd": str(tmp_path), "plan_path": str(plan)}],
    )
    assert _hold.hold_for_pr(42, str(tmp_path)) is None


def test_hold_check_cli_refuses_with_reason_and_setter(tmp_path, monkeypatch):
    _graph(
        tmp_path,
        monkeypatch,
        plan_body=(
            "---\nstatus: ready\ndispatch_hold:\n"
            "  reason: Blocking finding\n  release_when: Finding fixed\n"
            "  review_on: 2099-08-20\n  set_by: king:119e3c52\n---\n"
        ),
    )
    result = CliRunner().invoke(app, ["pr", "hold-check", "42", "--repo", str(tmp_path)])
    assert result.exit_code == 3, result.output
    assert "dispatch-hold:x-5a5c" in result.output
    assert "set_by=king:119e3c52" in result.output
