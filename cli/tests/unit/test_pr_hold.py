from __future__ import annotations

import json

from typer.testing import CliRunner

from fno.cli import app
from fno.graph.ladder import DispatchHoldState
from fno.pr import _hold
from fno.pr.closure import ClosureQueryError, PrClosureContext


def _graph(tmp_path, monkeypatch, *, plan_body: str, pr_body: str = "", entries=None):
    plan = tmp_path / "held.md"
    plan.write_text(plan_body)
    graph = tmp_path / "graph.json"
    resolved_entries = entries or [{
        "id": "x-5a5c",
        "cwd": str(tmp_path),
        "pr_number": 42,
        "pr_url": "https://github.com/o/r/pull/42",
        "plan_path": str(plan),
    }]
    graph.write_text(json.dumps({"entries": resolved_entries}))
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    # x-a93a: hold_for_pr scopes its "anything to check" gate to
    # detect_project(entries), which matches an entry's own `cwd` against
    # this repo's root (repo_root()) - the SAME lookup node intake uses, so
    # it can never disagree with a node's own `project` field. Every entry
    # here already carries cwd=str(tmp_path), so pinning repo_root() to
    # tmp_path is what makes that match land in a test sandbox.
    monkeypatch.setattr("fno.graph._intake.repo_root", lambda: str(tmp_path))
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


def test_hold_for_pr_fails_closed_on_a_closure_query_error_even_when_unstamped(
    tmp_path, monkeypatch
):
    """Round-11 review fix: a transient gh failure fetching the trailer must
    never read as "nothing to check" - an earlier version returned None here
    when nothing was ref-stamped, silently skipping the hold check for
    exactly the trailer-only case round-10 added this lookup to catch, just
    reachable via gh flakiness instead of a design gap."""
    plan = tmp_path / "unused.md"
    plan.write_text("---\nstatus: ready\n---\n")
    graph = tmp_path / "graph.json"
    graph.write_text(json.dumps({"entries": [
        {"id": "x-1111", "cwd": str(tmp_path), "plan_path": str(plan)},
    ]}))
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr("fno.graph._intake.repo_root", lambda: str(tmp_path))

    def _boom(pr_number, **k):
        raise ClosureQueryError("gh pr view timed out")

    monkeypatch.setattr("fno.pr.closure.fetch_pr_closure_context", _boom)
    reason = _hold.merge_hold_reason(42, str(tmp_path))
    assert reason is not None and "dispatch-hold-invalid:" in reason
    assert "PR body is unreadable" in reason


def test_hold_for_pr_returns_none_with_no_gh_call_when_repo_has_zero_backlog_nodes(
    tmp_path, monkeypatch,
):
    """x-a93a: graph.json is a single store shared across every project on
    the machine. A repo whose OWN root matches no entry's cwd must not pay
    the gh fetch just because SOME other project sharing the graph has
    nodes - and it must not be reachable at all, proving the short-circuit
    is real, not merely unexercised by this test's stub."""
    other_root = tmp_path / "other-project"
    other_root.mkdir()
    our_root = tmp_path / "our-repo"
    our_root.mkdir()
    plan = other_root / "other-project.md"
    plan.write_text("---\nstatus: ready\n---\n")
    graph = tmp_path / "graph.json"
    graph.write_text(json.dumps({"entries": [
        {"id": "x-2222", "cwd": str(other_root), "plan_path": str(plan)},
    ]}))
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr("fno.graph._intake.repo_root", lambda: str(our_root))

    def _unreachable(pr_number, **k):
        raise AssertionError("fetch_pr_closure_context must not be called")

    monkeypatch.setattr("fno.pr.closure.fetch_pr_closure_context", _unreachable)
    assert _hold.hold_for_pr(42, str(our_root)) is None


def test_hold_for_pr_still_checks_when_this_repos_project_has_a_ref_less_node(
    tmp_path, monkeypatch,
):
    """The project-scope short-circuit narrows, it never widens: a node
    whose cwd matches this repo's own root (even ref-less, unstamped) still
    reaches the gh fetch exactly as before - no regression to the
    round-10/11 trailer-only coverage. A positive control against test
    ``..._zero_backlog_nodes`` above: proves that test's None came from the
    project-scope guard, not from a mock that never gets exercised either way."""
    plan = tmp_path / "unheld.md"
    plan.write_text("---\nstatus: ready\n---\n")
    graph = tmp_path / "graph.json"
    graph.write_text(json.dumps({"entries": [
        {"id": "x-3333", "cwd": str(tmp_path), "plan_path": str(plan)},
    ]}))
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr("fno.graph._intake.repo_root", lambda: str(tmp_path))

    calls: list[int] = []

    def _spy(pr_number, **k):
        calls.append(pr_number)
        return PrClosureContext(
            number=pr_number, body="", url="https://github.com/o/r/pull/42",
            state="MERGED", merged_at="2026-01-01T00:00:00Z",
        )

    monkeypatch.setattr("fno.pr.closure.fetch_pr_closure_context", _spy)
    assert _hold.hold_for_pr(42, str(tmp_path)) is None
    assert calls == [42]


def test_hold_for_pr_still_checks_when_the_matching_node_has_a_null_project(
    tmp_path, monkeypatch,
):
    """Review fix: a node created via `fno backlog new`, not yet claimed by
    a plan, legitimately carries `project: null`. Matching on cwd (not on a
    resolved project-name string) means a null `project` field on the one
    node that DOES belong to this repo must not be misread as "no node
    here" - it still reaches the gh fetch."""
    plan = tmp_path / "unclaimed.md"
    plan.write_text("---\nstatus: ready\n---\n")
    graph = tmp_path / "graph.json"
    graph.write_text(json.dumps({"entries": [
        {"id": "x-4444", "cwd": str(tmp_path), "plan_path": str(plan), "project": None},
    ]}))
    monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
    monkeypatch.setattr("fno.graph._intake.repo_root", lambda: str(tmp_path))

    calls: list[int] = []

    def _spy(pr_number, **k):
        calls.append(pr_number)
        return PrClosureContext(
            number=pr_number, body="", url="https://github.com/o/r/pull/42",
            state="MERGED", merged_at="2026-01-01T00:00:00Z",
        )

    monkeypatch.setattr("fno.pr.closure.fetch_pr_closure_context", _spy)
    assert _hold.hold_for_pr(42, str(tmp_path)) is None
    assert calls == [42]


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
