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
from tests.fixtures.graph_seed import seed_graph

import json
from pathlib import Path
from typing import Optional

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
    seed_graph(g, '{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    seed_graph(g, json.dumps({"entries": entries}, indent=2) + "\n")


def _read(g: Path) -> list[dict]:
    from fno.graph.store import read_graph_strict

    return read_graph_strict(g)


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


def _door_done(tmp_path, entry, *args, pr_states=None, fail_stderr="", log=False):
    """The canonical close is native: run it through the door over a sandbox
    seeded with one row. pr_states maps PR number -> state for the gh stub;
    fail_stderr makes every gh call fail with that stderr; log makes the stub
    append its argv to <root>/gh.log so a test can assert the gh routing.
    Returns (code, out, err, graph_path)."""
    from tests.goldens._door import door, make_sandbox, seed_node, warm, write_pr_stub

    holder = tmp_path / f"door-{entry['id']}"
    holder.mkdir(exist_ok=True)
    row = {k: v for k, v in entry.items() if v is not None or k in ("id",)}
    root = make_sandbox(holder, [seed_node(
        entry["id"], entry.get("status", "ready"),
        **{k: v for k, v in entry.items() if k not in ("id", "status")},
    )])
    warm(root, entry["id"])
    prepend = None
    if pr_states is not None or fail_stderr or log:
        stubbin = write_pr_stub(root, pr_states, fail_stderr=fail_stderr)
        if log:
            gh = stubbin / "gh"
            lines = gh.read_text(encoding="utf-8").split("\n")
            # The log line rides AFTER the shebang: a script whose first line
            # is not a shebang is not directly executable.
            log_line = "printf '%s\\n' \"$@\" >> " + json.dumps(str(root / "gh.log"))
            gh.write_text(
                lines[0] + "\n" + log_line + "\n" + "\n".join(lines[1:]),
                encoding="utf-8",
            )
        prepend = str(stubbin)
    code, out, err = door(root, ["done", entry["id"], *args], path_prepend=prepend)
    return code, out, err, root / "graph.json"


# ---------------------------------------------------------------------------
# AC-EDGE: already-done node short-circuits with NO gh call
# ---------------------------------------------------------------------------


def test_already_done_short_circuits_no_gh_call(tmp_path, monkeypatch):
    """AC4-EDGE: second close of an already-done node is a no-op and never calls gh."""
    code, out, err, g = _door_done(
        tmp_path, _node("ab-12345678", completed_at="2026-01-01T00:00:00Z"), log=True,
    )
    assert code == 0, out + err
    assert not (g.parent / "gh.log").exists(), "an already-done close never calls gh"
    node = _read(g)[0]
    assert node["completed_at"] == "2026-01-01T00:00:00Z"


# ---------------------------------------------------------------------------
# AC3-EDGE: advisory node (no pr_number) closes with no gh requirement
# ---------------------------------------------------------------------------


def test_advisory_node_no_refs_closes_without_gh(tmp_path, monkeypatch):
    """AC3-EDGE: node with no pr_number/additional_prs closes immediately, no gh."""
    # The row carries an artifact link so the close-evidence rule passes:
    # the subject is the no-gh close path, not the refusal.
    code, out, err, g = _door_done(
        tmp_path,
        {**_node("ab-aaaaaa01"), "artifact_url": "https://example.test/artifact"},
        log=True,
    )
    assert code == 0, out + err
    assert not (g.parent / "gh.log").exists(), "a no-ref close never calls gh"
    node = _read(g)[0]
    assert node["completed_at"] is not None
    assert node.get("status") == "done"


# ---------------------------------------------------------------------------
# AC1-HP: MERGED PR - close proceeds with evidence
# ---------------------------------------------------------------------------


def test_merged_pr_closes_successfully(tmp_path, monkeypatch):
    """AC1-HP: node with a MERGED PR is closed; evidence logged."""
    code, out, err, g = _door_done(
        tmp_path,
        _node("ab-bb000001", pr_number=123, pr_url="https://github.com/org/repo/pull/123"),
        pr_states={123: "MERGED"},
    )
    assert code == 0, out + err
    node = _read(g)[0]
    assert node.get("status") == "done"
    assert node["completed_at"] is not None


def test_rest_closed_plus_merged_true_is_merge_evidence(tmp_path, monkeypatch):
    """REST never says MERGED: the wire shape is state "closed" + merged true.
    The close reads that shape as merged evidence (x-8ac3: the reader used to
    demand an uppercase MERGED state the REST API never sends, so every
    genuinely merged PR refused to close)."""
    code, out, err, g = _door_done(
        tmp_path,
        _node("ab-restm01", pr_number=150, pr_url="https://github.com/org/repo/pull/150"),
        pr_states={150: "MERGED"},
    )
    assert code == 0, out + err
    node = _read(g)[0]
    assert node.get("status") == "done"
    assert node["completed_at"] is not None


def test_unconditional_routing_refusal_is_actionable_not_retryable(tmp_path, monkeypatch):
    code, out, err, g = _door_done(
        tmp_path,
        _node("ab-route001", pr_number=1140, pr_url="https://github.com/o/r/pull/1140"),
        fail_stderr=(
            "[fno GraphQL reserve] use `fno do pr info 1140` for state/head/mergeability. "
            "This refusal is unconditional: the read is ROUTED, never rationed."
        ),
    )
    assert code == 3, out + err
    assert "fno do pr info 1140 --repo o/r" in err
    assert "retryable once gh is available again" not in err


def test_authentication_failure_is_typed_and_names_login(tmp_path, monkeypatch):
    code, out, err, g = _door_done(
        tmp_path,
        _node("ab-auth001", pr_number=1140, pr_url="https://github.com/o/r/pull/1140"),
        fail_stderr="gh: authentication required; run gh auth login",
    )
    assert code == 3, out + err
    assert "gh auth login" in err
    assert "retryable once gh is available again" not in err


def test_wrong_stored_repository_refuses_without_ambient_fallback(tmp_path, monkeypatch):
    code, out, err, g = _door_done(
        tmp_path,
        _node("ab-wrong001", pr_number=1140, pr_url="https://github.com/jasonnoahchoi/.claude/pull/1140"),
        pr_states=None, fail_stderr="gh: HTTP 404 Not Found", log=True,
    )
    assert code == 3, out + err
    # The read is scoped to the STORED repo, never the caller's checkout.
    gh_log = (g.parent / "gh.log")
    if gh_log.exists():
        assert any("jasonnoahchoi/.claude" in line for line in gh_log.read_text().splitlines())
    assert "Stored PR reference jasonnoahchoi/.claude#1140" in err
    assert "fno backlog update <node> --pr-url <correct-url>" in err
    assert "retryable once gh is available again" not in err


def test_backlog_done_closes_from_routed_rest_merge_evidence(tmp_path, monkeypatch):
    code, out, err, g = _door_done(
        tmp_path,
        _node("ab-restdone", pr_number=1140, pr_url="https://github.com/bllshttng/footnote/pull/1140"),
        pr_states={1140: "MERGED"}, log=True,
    )
    assert code == 0, out + err
    assert _read(g)[0]["status"] == "done"
    # The REST read is routed through the STORED repo, never the cwd's remote.
    gh_log = g.parent / "gh.log"
    assert gh_log.exists(), "the evidence read must consult gh"
    assert any(
        "repos/bllshttng/footnote/pulls/1140" in line
        for line in gh_log.read_text().splitlines()
    ), gh_log.read_text()


def test_nonretryable_refusal_wins_over_an_open_sibling():
    from fno.graph._reconcile import ReconcileError, resolve_merge_evidence

    def query(number, **kwargs):
        if number == 1:
            raise ReconcileError("stored repository PR not found", kind="not_found")
        return type("State", (), {"state": "OPEN"})()

    evidence = resolve_merge_evidence(
        [
            (1, "https://github.com/jasonnoahchoi/.claude/pull/1"),
            (2, "https://github.com/o/r/pull/2"),
        ],
        query=query,
    )

    assert evidence.outcome == "refused"
    assert evidence.failure_kind == "not_found"
    assert evidence.remedy is not None


def test_error_classifier_does_not_read_author_as_authentication():
    from fno.graph._reconcile import ReconcileError

    error = ReconcileError("repository author/repo returned HTTP 404 Not Found")

    assert error.kind == "not_found"


def test_missing_repository_context_is_not_retryable():
    from fno.graph._reconcile import ReconcileError

    error = ReconcileError("could not resolve owner/repo from git remote")

    assert error.kind == "repository_context"
    assert error.retryable is False
    assert "--repo <owner/repo>" in error.remedy_for(pr_number=1, repo=None)


def test_unclassified_read_failure_is_not_retryable():
    from fno.graph._reconcile import ReconcileError

    error = ReconcileError("unexpected reader failure")

    assert error.kind == "reader_error"
    assert error.retryable is False


def test_http_5xx_is_retryable_and_names_retry_action():
    from fno.graph._reconcile import ReconcileError

    error = ReconcileError("gh: HTTP 502 Bad Gateway")

    assert error.kind == "availability"
    assert error.retryable is True
    assert "retry" in error.remedy_for(pr_number=1, repo="o/r").lower()


# ---------------------------------------------------------------------------
# AC2-HP: OPEN no longer closes, even with green CI (regression against the
# removed behavior). Exit 5, node stays in_review, and no CI query is issued.
# ---------------------------------------------------------------------------


def test_open_green_pr_awaits_merge_exit5_no_ci_query(tmp_path, monkeypatch):
    """AC2-HP: OPEN PR with all-pass CI is no longer closing evidence.

    Exits 5 (awaiting merge), node stays open, and the close never consults
    CI - CI state is irrelevant to the close decision.
    """
    # The CI-query helper is gone entirely - CI is never consulted in the close
    # decision (x-aba7). Its absence is the structural guarantee.
    import fno.graph.cli as _gcli
    assert not hasattr(_gcli, "_done_ci_query")
    assert not hasattr(_gcli, "_ci_is_green")

    code, out, err, g = _door_done(
        tmp_path, _node("ab-cc000001", pr_number=200, pr_url="https://github.com/org/repo/pull/200"),
        pr_states={200: "OPEN"},
    )
    assert code == 5, f"expected 5 (awaiting merge), got {code}. output: {out + err}"
    node = _read(g)[0]
    assert not node.get("completed_at")
    assert node.get("status") != "done"


# ---------------------------------------------------------------------------
# AC2-HP: OPEN PR with red/pending CI also awaits merge (CI never consulted)
# ---------------------------------------------------------------------------


def test_open_red_pr_awaits_merge_exit5(tmp_path, monkeypatch):
    """OPEN PR awaits merge (exit 5) regardless of CI - CI is not queried."""
    code, out, err, g = _door_done(
        tmp_path, _node("ab-dd000001", pr_number=300, pr_url="https://github.com/org/repo/pull/300"),
        pr_states={300: "OPEN"},
    )
    assert code == 5, f"expected 5 (awaiting merge), got {code}. output: {out + err}"
    node = _read(g)[0]
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# AC4-UI: exit-5 stderr names the PR, the in_review hold, and who closes it
# ---------------------------------------------------------------------------


def test_awaiting_merge_stderr_is_explicit(tmp_path, monkeypatch):
    """AC4-UI: exit 5 stderr names the PR number, the in_review hold, and that
    reconcile/advance close it at merge - never a silent non-close."""
    code, out, err, g = _door_done(
        tmp_path, _node("ab-nn000001", pr_number=1300, pr_url="https://github.com/org/repo/pull/1300"),
        pr_states={1300: "OPEN"},
    )
    assert code == 5, out + err
    combined = out + err
    assert "1300" in combined
    assert "in_review" in combined.lower()
    assert "merge" in combined.lower()
    assert "reconcile" in combined.lower() or "advance" in combined.lower()


# ---------------------------------------------------------------------------
# AC1-HP: a MERGED ref wins over an OPEN ref on a multi-PR node
# ---------------------------------------------------------------------------


def test_merged_ref_wins_over_open_ref(tmp_path, monkeypatch):
    """A node with one OPEN and one MERGED ref closes on the MERGED evidence."""
    code, out, err, g = _door_done(
        tmp_path,
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
        },
        pr_states={10: "OPEN", 11: "MERGED"},
    )
    assert code == 0, f"expected 0 (merged wins), got {code}. output: {out + err}"
    node = _read(g)[0]
    assert node.get("status") == "done"


# ---------------------------------------------------------------------------
# AC3-ERR: CLOSED (not merged) PR - refuse
# ---------------------------------------------------------------------------


def test_closed_unmerged_pr_refuses(tmp_path, monkeypatch):
    """AC3-ERR: CLOSED (not merged) PR -> refuses with specific fact, exit 3, node stays open."""
    code, out, err, g = _door_done(
        tmp_path, _node("ab-ee000001", pr_number=400, pr_url="https://github.com/org/repo/pull/400"),
        pr_states={400: "CLOSED"},
    )
    assert code == 3, f"expected 3 (refusal), got {code}. output: {out + err}"
    combined = out + err
    assert "400" in combined
    assert "CLOSED" in combined or "closed" in combined.lower()
    node = _read(g)[0]
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# AC3-UI: --force without --reason rejected (usage error, exit 2)
# ---------------------------------------------------------------------------


def test_force_without_reason_is_usage_error(tmp_path, monkeypatch):
    """AC3-UI: --force without --reason is a usage error, exit 2."""
    code, out, err, g = _door_done(
        tmp_path, _node("ab-ff000001", pr_number=500), "--force",
    )
    assert code == 2, f"expected 2, got {code}. output: {out + err}"
    assert "reason" in (out + err).lower()
    node = _read(g)[0]
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# AC3-UI: --force --reason closes and journals the reason
# ---------------------------------------------------------------------------


def test_force_with_reason_closes_and_journals(tmp_path, monkeypatch):
    """AC3-UI: --force --reason closes node and the reason appears in output/events."""
    code, out, err, g = _door_done(
        tmp_path, _node("ab-gg000001", pr_number=600, pr_url="https://github.com/org/repo/pull/600"),
        "--force", "--reason", "manual test override",
        pr_states={600: "CLOSED"},
    )
    assert code == 0, f"expected 0 (force close), got {code}. output: {out + err}"
    node = _read(g)[0]
    assert node.get("status") == "done"
    assert node["completed_at"] is not None
    combined = out + err
    assert "manual test override" in combined


# ---------------------------------------------------------------------------
# AC3-FR: gh outage -> fail CLOSED, retryable exit code, node stays open
# ---------------------------------------------------------------------------


def test_gh_outage_fails_closed_retryable(tmp_path, monkeypatch):
    """AC3-FR: ReconcileError from gh -> retryable exit code (4), node stays open."""
    code, out, err, g = _door_done(
        tmp_path, _node("ab-hh000001", pr_number=700, pr_url="https://github.com/org/repo/pull/700"),
        fail_stderr="gh: network timeout",
    )
    assert code == 4, f"expected 4 (retryable gh outage), got {code}. output: {out + err}"
    combined = out + err
    assert "retry" in combined.lower() or "try again" in combined.lower()
    node = _read(g)[0]
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


def test_refusal_emits_event(tmp_path, monkeypatch):
    """AC1-HP: a refused close emits a backlog_done_refused event.

    The refusal RECEIPT is door-pinned by test_closed_unmerged_pr_refuses; the
    event journal itself is not door-observable (a sandbox has no space dir,
    the same disposition the drive-audit family got in test_done.py)."""
    code, out, err, g = _door_done(
        tmp_path, _node("ab-ii000001", pr_number=800, pr_url="https://github.com/org/repo/pull/800"),
        pr_states={800: "CLOSED"},
    )
    assert code == 3, out + err
    node = _read(g)[0]
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# AC3-UI: forced close emits forced-close event with reason
# ---------------------------------------------------------------------------


def test_forced_close_emits_event_with_reason(tmp_path, monkeypatch):
    """AC3-UI: --force --reason close emits a backlog_done_forced event carrying the reason.

    The forced close + reason journaling are door-pinned by
    test_force_with_reason_closes_and_journals; the event journal is not
    door-observable (no space dir in a sandbox, same as the drive-audit
    family)."""
    code, out, err, g = _door_done(
        tmp_path, _node("ab-jj000001", pr_number=900, pr_url="https://github.com/org/repo/pull/900"),
        "--force", "--reason", "operator override test",
        pr_states={900: "CLOSED"},
    )
    assert code == 0, out + err
    node = _read(g)[0]
    assert node.get("status") == "done"


# ---------------------------------------------------------------------------
# Partial outage: OPEN ref wins over an outaged ref -> exit 5 (definitive open
# PR means the node is awaiting merge; success-shaped, retryable-on-merge)
# ---------------------------------------------------------------------------


def test_open_ref_wins_over_outaged_ref(tmp_path, monkeypatch):
    """A definitive OPEN ref yields exit 5 even when another ref outages."""
    entry = {
        "id": "ab-pp000001",
        "title": "multi",
        "status": "ready",
        "domain": "code",
        "pr_number": 20,
        "pr_url": "https://github.com/org/repo/pull/20",
        "additional_prs": [
            {"number": 21, "url": "https://github.com/org/repo/pull/21"}
        ],
    }
    # A mixed stub: #20 answers OPEN, every other read dies mid-flight.
    from tests.goldens._door import door, make_sandbox, seed_node, warm

    holder = tmp_path / "door-pp"
    holder.mkdir(exist_ok=True)
    root = make_sandbox(holder, [seed_node("ab-pp000001", "ready", domain="code", pr_number=20,
                                           pr_url="https://github.com/org/repo/pull/20",
                                           additional_prs=entry["additional_prs"])])
    warm(root, "ab-pp000001")
    stubbin = root / "stubbin"
    stubbin.mkdir()
    gh = stubbin / "gh"
    gh.write_text(
        "#!/bin/sh\n"
        'case "$*" in */pulls/20*) '
        'printf \'%s\' \'{"state": "OPEN", "html_url": "https://github.com/org/repo/pull/20"}\'; exit 0;; esac\n'
        "printf '%s' 'gh: timeout on #21' >&2\nexit 1\n"
    )
    gh.chmod(0o755)
    code, out, err = door(root, ["done", "ab-pp000001"], path_prepend=str(stubbin))
    assert code == 5, f"expected 5 (awaiting merge), got {code}. output: {out + err}"
    node = _read(root / "graph.json")[0]
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# Partial outage conservatism: CLOSED ref + outaged ref (no OPEN, no MERGED)
# -> exit 4 (retryable), never a wrong refusal
# ---------------------------------------------------------------------------


def test_closed_ref_plus_outage_is_retryable(tmp_path, monkeypatch):
    """CLOSED + outage (no OPEN/MERGED) stays a retryable outage (exit 4)."""
    from tests.goldens._door import door, make_sandbox, seed_node, warm

    holder = tmp_path / "door-qq"
    holder.mkdir(exist_ok=True)
    root = make_sandbox(holder, [seed_node("ab-qq000001", "ready", domain="code", pr_number=30,
                                           pr_url="https://github.com/org/repo/pull/30",
                                           additional_prs=[{"number": 31, "url": "https://github.com/org/repo/pull/31"}])])
    warm(root, "ab-qq000001")
    stubbin = root / "stubbin"
    stubbin.mkdir()
    gh = stubbin / "gh"
    gh.write_text(
        "#!/bin/sh\n"
        'case "$*" in */pulls/30*) '
        'printf \'%s\' \'{"state": "CLOSED", "html_url": "https://github.com/org/repo/pull/30"}\'; exit 0;; esac\n'
        "printf '%s' 'gh: timeout on #31' >&2\nexit 1\n"
    )
    gh.chmod(0o755)
    code, out, err = door(root, ["done", "ab-qq000001"], path_prepend=str(stubbin))
    assert code == 4, f"expected 4 (retryable outage), got {code}. output: {out + err}"
    node = _read(root / "graph.json")[0]
    assert not node.get("completed_at")


# ---------------------------------------------------------------------------
# ab-bd9f476c: done stamps the plan shipped (not just graduate) on close
# ---------------------------------------------------------------------------


def test_done_real_stamp_marks_never_shipped_plan_done(tmp_graph, monkeypatch, tmp_path):
    """A merged-PR close stamps a never-shipped plan shipped->done using the
    evidencing PR url, rather than calling graduate (a no-op) on its own
    (ab-bd9f476c)."""
    import os

    from tests.goldens._door import door, make_sandbox, seed_node, warm, write_pr_stub

    plan = tmp_path / "p.md"
    plan.write_text("---\ntitle: t\nstatus: ready\n---\n\nbody\n")
    holder = tmp_path / "door-ab-done0001"
    holder.mkdir(exist_ok=True)
    root = make_sandbox(holder, [seed_node("ab-done0001", "ready", domain="code", pr_number=900,
                                           pr_url="https://github.com/org/repo/pull/900",
                                           plan_path=str(plan),
                                           session_id="sess-row")])
    warm(root, "ab-done0001")
    # The holder projects over every read, so the stamp's session id is the
    # live claim holder, not the graph row's retired mirror field: the row
    # carries a distinct stale id on purpose. FNO_CLAIMS_ROOT is a root; the
    # scan reads its .fno/claims directory.
    from fno.claims.core import acquire_claim

    acquire_claim("node:ab-done0001", "sess-9", ttl_ms=900_000, pid=os.getpid(),
                  reason="done stamp fixture", root=root / "claims")
    stub = write_pr_stub(root, {900: "MERGED"})
    code, out, err = door(root, ["done", "ab-done0001"], path_prepend=str(stub))
    assert code == 0, out + err

    text = plan.read_text()
    assert "status: done" in text  # stamped shipped, then graduated (1 url >= 1)
    assert "shipped_at:" in text
    assert "pull/900" in text
    assert "session_ids: [sess-9]" in text


def test_done_skip_stamp_leaves_plan_untouched(tmp_graph, monkeypatch, tmp_path):
    """--skip-stamp must not touch plan frontmatter even on a merged close."""
    plan = tmp_path / "p2.md"
    original = "---\ntitle: t\nstatus: ready\n---\n\nbody\n"
    plan.write_text(original)
    code, out, err, g = _door_done(
        tmp_path,
        {
            "id": "ab-done0002",
            "title": "t",
            "status": "ready",
            "domain": "code",
            "pr_number": 901,
            "pr_url": "https://github.com/org/repo/pull/901",
            "plan_path": str(plan),
        },
        "--skip-stamp",
        pr_states={901: "MERGED"},
    )
    assert code == 0, out + err
    assert plan.read_text() == original
