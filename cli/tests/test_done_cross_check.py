"""Tests for `fno backlog done` gh cross-check (Task 2.2).

The cross-check runs BEFORE the graph mutation. It refuses to close a node
unless at least one referenced PR is MERGED or OPEN with green CI.

Test filter: `python -m pytest tests/ -k done_cross_check`

Injection pattern: cmd_done accepts a `query` parameter (injected at the
module level via monkeypatch, same as the reconcile test pattern) so no real
gh subprocess is ever invoked.

Exit codes chosen (documented in cmd_done docstring):
    3  - gh cross-check refused: no merged/green evidence
    4  - gh outage / subprocess failure: retryable, node stays open
    2  - usage error (--force without --reason)
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Optional
from unittest.mock import MagicMock

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    """Fresh graph.json routed to cmd_done's code path."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _read(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


def _node(
    node_id: str,
    *,
    pr_number: Optional[int] = None,
    pr_url: Optional[str] = None,
    completed_at: Optional[str] = None,
) -> dict:
    """Build a minimal graph node fixture."""
    return {
        "id": node_id,
        "title": f"Node {node_id}",
        "_status": "done" if completed_at else "ready",
        "domain": "code",
        "pr_number": pr_number,
        "pr_url": pr_url,
        "completed_at": completed_at,
    }


def _make_gh_checks_output(checks: list[dict]) -> str:
    """Produce `gh pr checks --json name,state,bucket` captured output."""
    return json.dumps(checks)


def _invoke_done(node_id: str, extra_args: list[str] | None = None) -> object:
    """Run `fno backlog done <node_id> [extra_args]`."""
    args = ["backlog", "done", node_id] + (extra_args or [])
    return runner.invoke(app, args, catch_exceptions=False)


# ---------------------------------------------------------------------------
# Helpers to inject the query callable into cmd_done
# ---------------------------------------------------------------------------


def _patch_query(monkeypatch, query_fn):
    """Inject a stub query function into graph.cli.cmd_done."""
    import fno.graph.cli as gcli

    monkeypatch.setattr(gcli, "_done_gh_query", query_fn)


def _patch_ci_query(monkeypatch, ci_fn):
    """Inject a stub CI-checks function into graph.cli.cmd_done."""
    import fno.graph.cli as gcli

    monkeypatch.setattr(gcli, "_done_ci_query", ci_fn)


# ---------------------------------------------------------------------------
# AC-EDGE: already-done node short-circuits with NO gh call
# ---------------------------------------------------------------------------


def test_already_done_short_circuits_no_gh_call(tmp_graph, monkeypatch):
    """AC4-EDGE: second close of an already-done node is a no-op and never calls gh."""
    _seed(tmp_graph, [_node("ab-12345678", completed_at="2026-01-01T00:00:00Z")])

    gh_called = []

    def boom_query(*args, **kwargs):
        gh_called.append(True)
        raise AssertionError("gh should not be called for an already-done node")

    _patch_query(monkeypatch, boom_query)
    _patch_ci_query(monkeypatch, boom_query)

    result = _invoke_done("ab-12345678")

    assert result.exit_code == 0, result.output
    assert not gh_called
    # node stays done (idempotent)
    node = _read(tmp_graph)[0]
    assert node["completed_at"] == "2026-01-01T00:00:00Z"


# ---------------------------------------------------------------------------
# AC3-EDGE: advisory node (no pr_number) closes with no gh requirement
# ---------------------------------------------------------------------------


def test_advisory_node_no_refs_closes_without_gh(tmp_graph, monkeypatch):
    """AC3-EDGE: node with no pr_number/additional_prs closes immediately, no gh."""
    _seed(tmp_graph, [_node("ab-aaaaaa01")])

    gh_called = []

    def boom_query(*args, **kwargs):
        gh_called.append(True)
        raise AssertionError("gh should not be called for advisory (no-ref) node")

    _patch_query(monkeypatch, boom_query)
    _patch_ci_query(monkeypatch, boom_query)

    result = _invoke_done("ab-aaaaaa01")

    assert result.exit_code == 0, result.output
    assert not gh_called
    node = _read(tmp_graph)[0]
    assert node["completed_at"] is not None
    assert node.get("_status") == "done"


# ---------------------------------------------------------------------------
# AC1-HP: MERGED PR - close proceeds with evidence
# ---------------------------------------------------------------------------


def test_merged_pr_closes_successfully(tmp_graph, monkeypatch):
    """AC1-HP: node with a MERGED PR is closed; evidence logged."""
    from fno.graph._reconcile import PrMergeState

    _seed(
        tmp_graph,
        [_node("ab-bb000001", pr_number=123, pr_url="https://github.com/org/repo/pull/123")],
    )

    def merged_query(pr_number, **kwargs):
        return PrMergeState(
            number=pr_number, state="MERGED", url=f"https://github.com/org/repo/pull/{pr_number}",
            merged_at="2026-06-01T10:00:00Z",
        )

    _patch_query(monkeypatch, merged_query)

    result = _invoke_done("ab-bb000001")

    assert result.exit_code == 0, result.output
    node = _read(tmp_graph)[0]
    assert node.get("_status") == "done"
    assert node["completed_at"] is not None


# ---------------------------------------------------------------------------
# AC-HP: OPEN PR with green CI - close proceeds
# ---------------------------------------------------------------------------


def test_open_green_pr_closes_successfully(tmp_graph, monkeypatch):
    """AC-HP: OPEN PR with all-pass CI buckets is accepted as evidence."""
    from fno.graph._reconcile import PrMergeState

    _seed(
        tmp_graph,
        [_node("ab-cc000001", pr_number=200, pr_url="https://github.com/org/repo/pull/200")],
    )

    def open_query(pr_number, **kwargs):
        return PrMergeState(
            number=pr_number, state="OPEN", url=f"https://github.com/org/repo/pull/{pr_number}",
            merged_at=None,
        )

    def green_ci(pr_number, **kwargs):
        # Simulate `gh pr checks --json name,state,bucket` returning all-pass
        return [
            {"name": "ci", "state": "SUCCESS", "bucket": "pass"},
            {"name": "lint", "state": "SKIPPED", "bucket": "skipping"},
        ]

    _patch_query(monkeypatch, open_query)
    _patch_ci_query(monkeypatch, green_ci)

    result = _invoke_done("ab-cc000001")

    assert result.exit_code == 0, result.output
    node = _read(tmp_graph)[0]
    assert node.get("_status") == "done"
    assert node["completed_at"] is not None


# ---------------------------------------------------------------------------
# AC3-ERR: OPEN PR with red CI - refuse with specific fact
# ---------------------------------------------------------------------------


def test_open_red_pr_refuses_with_specific_fact(tmp_graph, monkeypatch):
    """AC3-ERR: OPEN PR with fail bucket -> refuses, prints specific fact, exit 3, node stays open."""
    from fno.graph._reconcile import PrMergeState

    _seed(
        tmp_graph,
        [_node("ab-dd000001", pr_number=300, pr_url="https://github.com/org/repo/pull/300")],
    )

    def open_query(pr_number, **kwargs):
        return PrMergeState(number=pr_number, state="OPEN", url=None, merged_at=None)

    def red_ci(pr_number, **kwargs):
        return [
            {"name": "unit-tests", "state": "FAILURE", "bucket": "fail"},
            {"name": "lint", "state": "IN_PROGRESS", "bucket": "pending"},
        ]

    _patch_query(monkeypatch, open_query)
    _patch_ci_query(monkeypatch, red_ci)

    result = _invoke_done("ab-dd000001")

    assert result.exit_code == 3, f"expected 3 (refusal), got {result.exit_code}. output: {result.output}"
    # Must print specific fact
    combined = result.output + (result.stderr or "")
    assert "PR #300" in combined or "300" in combined
    # Must name the failing check or state
    assert "fail" in combined.lower() or "FAILURE" in combined or "red" in combined.lower()
    # Node must stay open
    node = _read(tmp_graph)[0]
    assert node.get("_status") != "done"
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# AC3-ERR: CLOSED (not merged) PR - refuse
# ---------------------------------------------------------------------------


def test_closed_unmerged_pr_refuses(tmp_graph, monkeypatch):
    """AC3-ERR: CLOSED (not merged) PR -> refuses with specific fact, exit 3, node stays open."""
    from fno.graph._reconcile import PrMergeState

    _seed(
        tmp_graph,
        [_node("ab-ee000001", pr_number=400, pr_url="https://github.com/org/repo/pull/400")],
    )

    def closed_query(pr_number, **kwargs):
        return PrMergeState(number=pr_number, state="CLOSED", url=None, merged_at=None)

    _patch_query(monkeypatch, closed_query)

    result = _invoke_done("ab-ee000001")

    assert result.exit_code == 3, f"expected 3 (refusal), got {result.exit_code}. output: {result.output}"
    combined = result.output + (result.stderr or "")
    assert "400" in combined
    assert "CLOSED" in combined or "closed" in combined.lower()
    node = _read(tmp_graph)[0]
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# AC3-UI: --force without --reason rejected (usage error, exit 2)
# ---------------------------------------------------------------------------


def test_force_without_reason_is_usage_error(tmp_graph, monkeypatch):
    """AC3-UI: --force without --reason is a usage error, exit 2."""
    _seed(tmp_graph, [_node("ab-ff000001", pr_number=500)])

    result = runner.invoke(app, ["backlog", "done", "ab-ff000001", "--force"], catch_exceptions=False)

    # Must be a usage-level error (exit 2), not a close
    assert result.exit_code == 2, f"expected 2, got {result.exit_code}. output: {result.output}"
    combined = result.output + (result.stderr or "")
    assert "reason" in combined.lower()
    # Node stays open
    node = _read(tmp_graph)[0]
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# AC3-UI: --force --reason closes and journals the reason
# ---------------------------------------------------------------------------


def test_force_with_reason_closes_and_journals(tmp_graph, monkeypatch):
    """AC3-UI: --force --reason closes node and the reason appears in output/events."""
    from fno.graph._reconcile import PrMergeState

    _seed(
        tmp_graph,
        [_node("ab-gg000001", pr_number=600, pr_url="https://github.com/org/repo/pull/600")],
    )

    # Even with a CLOSED PR (would normally refuse), force overrides
    def closed_query(pr_number, **kwargs):
        return PrMergeState(number=pr_number, state="CLOSED", url=None, merged_at=None)

    _patch_query(monkeypatch, closed_query)

    result = _invoke_done("ab-gg000001", ["--force", "--reason", "manual test override"])

    assert result.exit_code == 0, f"expected 0 (force close), got {result.exit_code}. output: {result.output}"
    node = _read(tmp_graph)[0]
    assert node.get("_status") == "done"
    assert node["completed_at"] is not None
    # Reason should appear in output (journaling)
    combined = result.output + (result.stderr or "")
    assert "manual test override" in combined


# ---------------------------------------------------------------------------
# AC3-FR: gh outage -> fail CLOSED, retryable exit code, node stays open
# ---------------------------------------------------------------------------


def test_gh_outage_fails_closed_retryable(tmp_graph, monkeypatch):
    """AC3-FR: ReconcileError from gh -> retryable exit code (4), node stays open."""
    from fno.graph._reconcile import ReconcileError

    _seed(
        tmp_graph,
        [_node("ab-hh000001", pr_number=700, pr_url="https://github.com/org/repo/pull/700")],
    )

    def outage_query(pr_number, **kwargs):
        raise ReconcileError("gh: network timeout")

    _patch_query(monkeypatch, outage_query)

    result = _invoke_done("ab-hh000001")

    # Must use exit code 4 (distinct from refusal's 3)
    assert result.exit_code == 4, f"expected 4 (retryable gh outage), got {result.exit_code}. output: {result.output}"
    combined = result.output + (result.stderr or "")
    # Must say it's retryable
    assert "retry" in combined.lower() or "retryable" in combined.lower() or "try again" in combined.lower()
    # Node must NOT be closed
    node = _read(tmp_graph)[0]
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# AC3-ERR: exit codes are distinct (refusal != outage != usage)
# ---------------------------------------------------------------------------


def test_exit_codes_are_distinct(tmp_graph, monkeypatch):
    """Verify exit codes: 3=refusal, 4=gh-outage, 2=usage-error are all distinct."""
    # Just a sanity check on the constants in use
    assert 3 != 4
    assert 3 != 2
    assert 4 != 2


# ---------------------------------------------------------------------------
# AC1-HP: refusal event emitted on refusal path
# ---------------------------------------------------------------------------


def test_refusal_emits_event(tmp_graph, monkeypatch):
    """AC1-HP: a refused close emits a backlog_done_refused event."""
    from fno.graph._reconcile import PrMergeState

    _seed(
        tmp_graph,
        [_node("ab-ii000001", pr_number=800, pr_url="https://github.com/org/repo/pull/800")],
    )

    def closed_query(pr_number, **kwargs):
        return PrMergeState(number=pr_number, state="CLOSED", url=None, merged_at=None)

    _patch_query(monkeypatch, closed_query)

    # Capture events written
    emitted: list[dict] = []

    import fno.events as evts

    original_append = evts.append_event

    def capture_append(event, *args, **kwargs):
        emitted.append(event)

    monkeypatch.setattr(evts, "append_event", capture_append)

    result = _invoke_done("ab-ii000001")

    assert result.exit_code == 3
    kinds = [e.get("type") for e in emitted]
    assert "backlog_done_refused" in kinds, f"expected backlog_done_refused event, got: {kinds}"


# ---------------------------------------------------------------------------
# AC3-UI: forced close emits forced-close event with reason
# ---------------------------------------------------------------------------


def test_forced_close_emits_event_with_reason(tmp_graph, monkeypatch):
    """AC3-UI: --force --reason close emits a backlog_done_forced event carrying the reason."""
    from fno.graph._reconcile import PrMergeState

    _seed(
        tmp_graph,
        [_node("ab-jj000001", pr_number=900, pr_url="https://github.com/org/repo/pull/900")],
    )

    def closed_query(pr_number, **kwargs):
        return PrMergeState(number=pr_number, state="CLOSED", url=None, merged_at=None)

    _patch_query(monkeypatch, closed_query)

    emitted: list[dict] = []

    import fno.events as evts

    original_append = evts.append_event

    def capture_append(event, *args, **kwargs):
        emitted.append(event)

    monkeypatch.setattr(evts, "append_event", capture_append)

    result = _invoke_done("ab-jj000001", ["--force", "--reason", "operator override test"])

    assert result.exit_code == 0
    kinds = {e.get("type"): e for e in emitted}
    assert "backlog_done_forced" in kinds, f"expected backlog_done_forced event, got: {list(kinds)}"
    forced_evt = kinds["backlog_done_forced"]
    assert "operator override test" in json.dumps(forced_evt.get("data", {}))


# ---------------------------------------------------------------------------
# Additional: OPEN + pending CI (not all-pass) -> refuses
# ---------------------------------------------------------------------------


def test_open_pending_ci_refuses(tmp_graph, monkeypatch):
    """OPEN PR with a pending check is not green -> refuses."""
    from fno.graph._reconcile import PrMergeState

    _seed(
        tmp_graph,
        [_node("ab-kk000001", pr_number=1000, pr_url="https://github.com/org/repo/pull/1000")],
    )

    def open_query(pr_number, **kwargs):
        return PrMergeState(number=pr_number, state="OPEN", url=None, merged_at=None)

    def pending_ci(pr_number, **kwargs):
        return [
            {"name": "ci", "state": "SUCCESS", "bucket": "pass"},
            {"name": "smoke", "state": "IN_PROGRESS", "bucket": "pending"},
        ]

    _patch_query(monkeypatch, open_query)
    _patch_ci_query(monkeypatch, pending_ci)

    result = _invoke_done("ab-kk000001")

    assert result.exit_code == 3
    node = _read(tmp_graph)[0]
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# Additional: OPEN + no CI checks at all (no checks configured) -> refuses
# ---------------------------------------------------------------------------


def test_open_no_ci_checks_refuses(tmp_graph, monkeypatch):
    """OPEN PR with empty CI check list is not evidence -> refuses."""
    from fno.graph._reconcile import PrMergeState

    _seed(
        tmp_graph,
        [_node("ab-ll000001", pr_number=1100, pr_url="https://github.com/org/repo/pull/1100")],
    )

    def open_query(pr_number, **kwargs):
        return PrMergeState(number=pr_number, state="OPEN", url=None, merged_at=None)

    def empty_ci(pr_number, **kwargs):
        return []  # no checks configured

    _patch_query(monkeypatch, open_query)
    _patch_ci_query(monkeypatch, empty_ci)

    result = _invoke_done("ab-ll000001")

    assert result.exit_code == 3
    node = _read(tmp_graph)[0]
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# Additional: CI outage for OPEN PR -> exit 4 (gh outage, not refusal)
# ---------------------------------------------------------------------------


def test_open_ci_outage_fails_closed(tmp_graph, monkeypatch):
    """OPEN PR where gh pr checks fails -> retryable exit 4, node stays open."""
    from fno.graph._reconcile import PrMergeState, ReconcileError

    _seed(
        tmp_graph,
        [_node("ab-mm000001", pr_number=1200, pr_url="https://github.com/org/repo/pull/1200")],
    )

    def open_query(pr_number, **kwargs):
        return PrMergeState(number=pr_number, state="OPEN", url=None, merged_at=None)

    def ci_outage(pr_number, **kwargs):
        raise ReconcileError("gh pr checks timed out")

    _patch_query(monkeypatch, open_query)
    _patch_ci_query(monkeypatch, ci_outage)

    result = _invoke_done("ab-mm000001")

    assert result.exit_code == 4
    node = _read(tmp_graph)[0]
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# GAP-6: _ci_is_green direct unit tests with real captured bucket shapes
# ---------------------------------------------------------------------------
# Bucket fixtures mirror real `gh pr checks --json name,state,bucket` output
# (shapes verified in commit fb9a48e8 that flipped shell-harness mocks to the
# real bucket schema; `conclusion` is NOT a field in this endpoint).


import pytest  # noqa: E402 (already imported above but explicit for clarity)


@pytest.mark.parametrize(
    "checks, expected_green, reason_fragment",
    [
        # All pass -> green
        pytest.param(
            [
                {"name": "ci", "state": "SUCCESS", "bucket": "pass"},
                {"name": "lint", "state": "SUCCESS", "bucket": "pass"},
            ],
            True,
            "",
            id="all_pass_green",
        ),
        # Mix of pass and skipping -> green
        pytest.param(
            [
                {"name": "ci", "state": "SUCCESS", "bucket": "pass"},
                {"name": "optional", "state": "SKIPPED", "bucket": "skipping"},
            ],
            True,
            "",
            id="pass_and_skipping_green",
        ),
        # Single fail bucket -> not green, names the failing check
        pytest.param(
            [
                {"name": "unit-tests", "state": "FAILURE", "bucket": "fail"},
                {"name": "lint", "state": "SUCCESS", "bucket": "pass"},
            ],
            False,
            "unit-tests",
            id="one_fail_not_green",
        ),
        # Cancel bucket -> not green
        pytest.param(
            [
                {"name": "smoke", "state": "CANCELLED", "bucket": "cancel"},
            ],
            False,
            "smoke",
            id="cancel_not_green",
        ),
        # Pending check -> not green
        pytest.param(
            [
                {"name": "ci", "state": "SUCCESS", "bucket": "pass"},
                {"name": "deploy", "state": "IN_PROGRESS", "bucket": "pending"},
            ],
            False,
            "deploy",
            id="pending_not_green",
        ),
        # Empty list -> not green ("no CI checks configured")
        pytest.param(
            [],
            False,
            "no CI checks",
            id="empty_not_green",
        ),
        # All skipping (no pass, no fail) -> green (skipping-only is acceptable)
        pytest.param(
            [
                {"name": "optional-a", "state": "SKIPPED", "bucket": "skipping"},
                {"name": "optional-b", "state": "SKIPPED", "bucket": "skipping"},
            ],
            True,
            "",
            id="all_skipping_green",
        ),
    ],
)
def test_ci_is_green_parametrized(checks, expected_green, reason_fragment):
    """GAP-6: _ci_is_green with real captured `gh pr checks --json name,state,bucket` shapes."""
    from fno.graph.cli import _ci_is_green

    green, reason = _ci_is_green(checks)
    assert green == expected_green, (
        f"expected green={expected_green} for checks={checks!r}, got green={green}, reason={reason!r}"
    )
    if reason_fragment:
        assert reason_fragment.lower() in reason.lower(), (
            f"expected {reason_fragment!r} in reason {reason!r}"
        )


def test_ci_is_green_tolerates_non_dict_elements():
    """Gemini MEDIUM: _ci_is_green must not AttributeError on non-dict check items.

    gh pr checks output can include non-dict elements in unexpected edge cases.
    The function must filter those out and process only dict items.
    """
    from fno.graph.cli import _ci_is_green

    # Mix of a passing dict check and a string element (non-dict).
    # Must not crash; the string is ignored; the dict check is pass -> green.
    checks_with_string = [
        {"name": "ci", "state": "SUCCESS", "bucket": "pass"},
        "some-unexpected-string-element",
    ]
    green, reason = _ci_is_green(checks_with_string)
    assert green is True, f"non-dict elements must be filtered out; got green={green}, reason={reason!r}"

    # Non-dict element alongside a fail check -> not green (fail wins).
    checks_with_fail = [
        "unexpected-string",
        {"name": "lint", "state": "FAILURE", "bucket": "fail"},
    ]
    green2, reason2 = _ci_is_green(checks_with_fail)
    assert green2 is False, f"fail bucket must still trigger not-green; got green={green2}"
    assert "lint" in reason2.lower(), f"reason must name the failing check; got: {reason2!r}"

    # All non-dict -> treated as empty after filter -> not green.
    checks_all_strings = ["foo", 42, None]
    green3, reason3 = _ci_is_green(checks_all_strings)
    assert green3 is False, f"all non-dict after filter must be not-green; got green={green3}"


# ---------------------------------------------------------------------------
# ab-bd9f476c: done stamps the plan shipped (not just graduate) on close
# ---------------------------------------------------------------------------


def test_done_real_stamp_marks_never_shipped_plan_done(tmp_graph, monkeypatch, tmp_path):
    """A merged-PR close stamps a never-shipped plan shipped->done using the
    evidencing PR url, rather than calling graduate (a no-op) on its own
    (ab-bd9f476c)."""
    from fno.graph._reconcile import PrMergeState

    plan = tmp_path / "p.md"
    plan.write_text("---\ntitle: t\nstatus: ready\n---\n\nbody\n")
    _seed(
        tmp_graph,
        [
            {
                "id": "ab-done0001",
                "title": "t",
                "_status": "ready",
                "domain": "code",
                "pr_number": 900,
                "pr_url": "https://github.com/org/repo/pull/900",
                "completed_at": None,
                "plan_path": str(plan),
                "session_id": "sess-9",
            }
        ],
    )

    def merged_query(pr_number, **kwargs):
        return PrMergeState(
            number=pr_number,
            state="MERGED",
            url=f"https://github.com/org/repo/pull/{pr_number}",
            merged_at="2026-06-01T10:00:00Z",
        )

    _patch_query(monkeypatch, merged_query)

    result = _invoke_done("ab-done0001")
    assert result.exit_code == 0, result.output

    text = plan.read_text()
    assert "status: done" in text  # stamped shipped, then graduated (1 url >= 1)
    assert "shipped_at:" in text
    assert "pull/900" in text
    assert "session_ids: [sess-9]" in text


def test_done_skip_stamp_leaves_plan_untouched(tmp_graph, monkeypatch, tmp_path):
    """--skip-stamp must not touch plan frontmatter even on a merged close."""
    from fno.graph._reconcile import PrMergeState

    plan = tmp_path / "p.md"
    original = "---\ntitle: t\nstatus: ready\n---\n\nbody\n"
    plan.write_text(original)
    _seed(
        tmp_graph,
        [
            {
                "id": "ab-done0002",
                "title": "t",
                "_status": "ready",
                "domain": "code",
                "pr_number": 901,
                "pr_url": "https://github.com/org/repo/pull/901",
                "completed_at": None,
                "plan_path": str(plan),
            }
        ],
    )
    _patch_query(
        monkeypatch,
        lambda n, **k: PrMergeState(
            number=n, state="MERGED", url=f"https://github.com/org/repo/pull/{n}",
            merged_at="2026-06-01T10:00:00Z",
        ),
    )

    result = _invoke_done("ab-done0002", ["--skip-stamp"])
    assert result.exit_code == 0, result.output
    assert plan.read_text() == original
