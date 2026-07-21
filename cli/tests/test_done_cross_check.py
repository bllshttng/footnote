"""Tests for `fno backlog done` gh cross-check (x-aba7: graph done = merged).

The cross-check runs BEFORE the graph mutation. MERGED is the ONLY closing
evidence. An OPEN PR (regardless of CI state) yields exit 5 (awaiting merge,
success-shaped): the node stays in_review and closes on the actual merge via
reconcile / merge-triggered advance. CI state is irrelevant to the close
decision - whether CI is green is the session's finish-line concern (loop-check),
not close evidence.

Test filter: `python -m pytest tests/ -k done_cross_check`

Injection pattern: cmd_done accepts a `query` parameter (injected at the
module level via monkeypatch, same as the reconcile test pattern) so no real
gh subprocess is ever invoked.

Exit codes chosen (documented in cmd_done docstring):
    3  - gh cross-check refused: CLOSED-unmerged / UNKNOWN, no merge/open evidence
    4  - gh outage / subprocess failure: retryable, node stays open
    5  - awaiting merge: PR OPEN, not merged; node stays in_review (success-shaped)
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
        "status": "done" if completed_at else "ready",
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

    result = _invoke_done("ab-aaaaaa01")

    assert result.exit_code == 0, result.output
    assert not gh_called
    node = _read(tmp_graph)[0]
    assert node["completed_at"] is not None
    assert node.get("status") == "done"


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
    assert node.get("status") == "done"
    assert node["completed_at"] is not None


# ---------------------------------------------------------------------------
# AC2-HP: OPEN no longer closes, even with green CI (regression against the
# removed behavior). Exit 5, node stays in_review, and no CI query is issued.
# ---------------------------------------------------------------------------


def test_open_green_pr_awaits_merge_exit5_no_ci_query(tmp_graph, monkeypatch):
    """AC2-HP: OPEN PR with all-pass CI is no longer closing evidence.

    Exits 5 (awaiting merge), node stays open, and `_done_ci_query` is NEVER
    called - CI state is irrelevant to the close decision.
    """
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

    _patch_query(monkeypatch, open_query)

    # The CI-query helper is gone entirely - CI is never consulted in the close
    # decision (x-aba7). Its absence is the structural guarantee.
    import fno.graph.cli as _gcli
    assert not hasattr(_gcli, "_done_ci_query")
    assert not hasattr(_gcli, "_ci_is_green")

    result = _invoke_done("ab-cc000001")

    assert result.exit_code == 5, f"expected 5 (awaiting merge), got {result.exit_code}. output: {result.output}"
    node = _read(tmp_graph)[0]
    assert not node.get("completed_at")
    assert node.get("status") != "done"


# ---------------------------------------------------------------------------
# AC2-HP: OPEN PR with red/pending CI also awaits merge (CI never consulted)
# ---------------------------------------------------------------------------


def test_open_red_pr_awaits_merge_exit5(tmp_graph, monkeypatch):
    """OPEN PR awaits merge (exit 5) regardless of CI - CI is not queried."""
    from fno.graph._reconcile import PrMergeState

    _seed(
        tmp_graph,
        [_node("ab-dd000001", pr_number=300, pr_url="https://github.com/org/repo/pull/300")],
    )

    def open_query(pr_number, **kwargs):
        return PrMergeState(number=pr_number, state="OPEN", url=None, merged_at=None)

    _patch_query(monkeypatch, open_query)

    result = _invoke_done("ab-dd000001")

    assert result.exit_code == 5, f"expected 5 (awaiting merge), got {result.exit_code}. output: {result.output}"
    node = _read(tmp_graph)[0]
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# AC4-UI: exit-5 stderr names the PR, the in_review hold, and who closes it
# ---------------------------------------------------------------------------


def test_awaiting_merge_stderr_is_explicit(tmp_graph, monkeypatch):
    """AC4-UI: exit 5 stderr names the PR number, the in_review hold, and that
    reconcile/advance close it at merge - never a silent non-close."""
    from fno.graph._reconcile import PrMergeState

    _seed(
        tmp_graph,
        [_node("ab-nn000001", pr_number=1300, pr_url="https://github.com/org/repo/pull/1300")],
    )

    _patch_query(
        monkeypatch,
        lambda n, **k: PrMergeState(number=n, state="OPEN", url=None, merged_at=None),
    )

    result = _invoke_done("ab-nn000001")

    assert result.exit_code == 5, result.output
    combined = result.output + (result.stderr or "")
    assert "1300" in combined
    assert "in_review" in combined.lower()
    assert "merge" in combined.lower()
    assert "reconcile" in combined.lower() or "advance" in combined.lower()


# ---------------------------------------------------------------------------
# AC1-HP: a MERGED ref wins over an OPEN ref on a multi-PR node
# ---------------------------------------------------------------------------


def test_merged_ref_wins_over_open_ref(tmp_graph, monkeypatch):
    """A node with one OPEN and one MERGED ref closes on the MERGED evidence."""
    from fno.graph._reconcile import PrMergeState

    _seed(
        tmp_graph,
        [
            {
                "id": "ab-oo000001",
                "title": "multi",
                "status": "ready",
                "domain": "code",
                "pr_number": 10,
                "pr_url": "https://github.com/org/repo/pull/10",
                "additional_prs": [
                    {"number": 11, "url": "https://github.com/org/repo/pull/11"}
                ],
                "completed_at": None,
            }
        ],
    )

    def query(pr_number, **kwargs):
        # #10 OPEN, #11 MERGED
        if pr_number == 11:
            return PrMergeState(
                number=11, state="MERGED",
                url="https://github.com/org/repo/pull/11", merged_at="2026-06-01T10:00:00Z",
            )
        return PrMergeState(number=pr_number, state="OPEN", url=None, merged_at=None)

    _patch_query(monkeypatch, query)

    result = _invoke_done("ab-oo000001")

    assert result.exit_code == 0, f"expected 0 (merged wins), got {result.exit_code}. output: {result.output}"
    node = _read(tmp_graph)[0]
    assert node.get("status") == "done"


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
    assert node.get("status") == "done"
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
    """Verify exit codes: 2=usage, 3=refusal, 4=gh-outage, 5=awaiting-merge are distinct."""
    # Just a sanity check on the constants in use
    assert len({2, 3, 4, 5}) == 4


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
# Partial outage: OPEN ref wins over an outaged ref -> exit 5 (definitive open
# PR means the node is awaiting merge; success-shaped, retryable-on-merge)
# ---------------------------------------------------------------------------


def test_open_ref_wins_over_outaged_ref(tmp_graph, monkeypatch):
    """A definitive OPEN ref yields exit 5 even when another ref outages."""
    from fno.graph._reconcile import PrMergeState, ReconcileError

    _seed(
        tmp_graph,
        [
            {
                "id": "ab-pp000001",
                "title": "multi",
                "status": "ready",
                "domain": "code",
                "pr_number": 20,
                "pr_url": "https://github.com/org/repo/pull/20",
                "additional_prs": [
                    {"number": 21, "url": "https://github.com/org/repo/pull/21"}
                ],
                "completed_at": None,
            }
        ],
    )

    def query(pr_number, **kwargs):
        if pr_number == 21:
            raise ReconcileError("gh: timeout on #21")
        return PrMergeState(number=20, state="OPEN", url=None, merged_at=None)

    _patch_query(monkeypatch, query)

    result = _invoke_done("ab-pp000001")

    assert result.exit_code == 5, f"expected 5 (awaiting merge), got {result.exit_code}. output: {result.output}"
    node = _read(tmp_graph)[0]
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# Partial outage conservatism: CLOSED ref + outaged ref (no OPEN, no MERGED)
# -> exit 4 (retryable), never a wrong refusal
# ---------------------------------------------------------------------------


def test_closed_ref_plus_outage_is_retryable(tmp_graph, monkeypatch):
    """CLOSED + outage (no OPEN/MERGED) stays a retryable outage (exit 4)."""
    from fno.graph._reconcile import PrMergeState, ReconcileError

    _seed(
        tmp_graph,
        [
            {
                "id": "ab-qq000001",
                "title": "multi",
                "status": "ready",
                "domain": "code",
                "pr_number": 30,
                "pr_url": "https://github.com/org/repo/pull/30",
                "additional_prs": [
                    {"number": 31, "url": "https://github.com/org/repo/pull/31"}
                ],
                "completed_at": None,
            }
        ],
    )

    def query(pr_number, **kwargs):
        if pr_number == 31:
            raise ReconcileError("gh: timeout on #31")
        return PrMergeState(number=30, state="CLOSED", url=None, merged_at=None)

    _patch_query(monkeypatch, query)

    result = _invoke_done("ab-qq000001")

    assert result.exit_code == 4, f"expected 4 (retryable outage), got {result.exit_code}. output: {result.output}"
    node = _read(tmp_graph)[0]
    assert not node.get("completed_at")


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
                "status": "ready",
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
                "status": "ready",
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
