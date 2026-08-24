"""Unit tests for the exact Backlog-Closure trailer (x-59a6).

Covers AC1-HP/EDGE, AC2-HP/EDGE, AC3-HP/EDGE/ERR, AC4-EDGE (idempotent rebind).
"""
from __future__ import annotations

import json
from types import SimpleNamespace

import pytest

from fno.pr.closure import (
    bind_created_pr,
    ClosureQueryError,
    bind_closure_claims,
    contained_descendant_ids,
    fetch_pr_closure_context,
    parse_closure_trailer,
    render_closure_trailer,
    render_pr_closure_trailer,
)


def _node(**kw) -> dict:
    base = {"id": "x-0001", "status": "ready"}
    base.update(kw)
    return base


def test_bind_created_pr_maps_one_real_branch_node_and_owner():
    entries = [_node(id="x-38e0"), _node(id="x-9999")]

    result = bind_created_pr(
        entries,
        head_ref="feature/x-38e0-live-node",
        pr_url="https://github.com/o/r/pull/1038",
        owner="worker-session",
    )

    assert result.outcome == "bound"
    assert entries[0]["pr_number"] == 1038
    assert entries[0]["pr_url"] == "https://github.com/o/r/pull/1038"
    assert entries[0]["locked_by"] == "worker-session"
    assert entries[0]["session_id"] == "worker-session"


def test_bind_created_pr_is_idempotent():
    entries = [_node(id="x-38e0")]
    kwargs = {
        "head_ref": "feature/x-38e0-live-node",
        "pr_url": "https://github.com/o/r/pull/1038",
        "owner": "worker-session",
    }

    first = bind_created_pr(entries, **kwargs)
    snapshot = [dict(entry) for entry in entries]
    second = bind_created_pr(entries, **kwargs)

    assert first.outcome == second.outcome == "bound"
    assert entries == snapshot


def test_bind_created_pr_refuses_unknown_ambiguous_and_malformed_without_mutation():
    cases = [
        ("feature/x-dead", "https://github.com/o/r/pull/1038"),
        ("feature/x-38e0/x-9999", "https://github.com/o/r/pull/1038"),
        ("feature/x-38e0", "not-a-pr-url"),
    ]
    for head_ref, pr_url in cases:
        entries = [_node(id="x-38e0"), _node(id="x-9999")]
        snapshot = [dict(entry) for entry in entries]

        result = bind_created_pr(entries, head_ref=head_ref, pr_url=pr_url, owner="worker-session")

        assert result.outcome == "refused"
        assert entries == snapshot


# ---------------------------------------------------------------------------
# parse_closure_trailer
# ---------------------------------------------------------------------------


def test_parse_exact_trailer_two_ids():
    body = "Fixes the thing.\n\nBacklog-Closure: x-5b99 x-62a1\n"
    assert parse_closure_trailer(body) == ["x-5b99", "x-62a1"]


def test_parse_ignores_prose_mentions():
    # AC2-EDGE / AC1-EDGE style: a dependency/collision sentence naming an id
    # is never a claim, even with "closes" in it, unless it is the exact line.
    body = (
        "## Dependencies\n"
        "x-3a91 is blocked_by this and gets its own Part 2 plan.\n"
        "## Collisions\n"
        "branch x-3a91 (client.rs, unmerged) are untouched.\n"
    )
    assert parse_closure_trailer(body) == []


def test_parse_dedupes_and_preserves_order():
    body = "Backlog-Closure: x-aaaa x-bbbb x-aaaa\n"
    assert parse_closure_trailer(body) == ["x-aaaa", "x-bbbb"]


def test_parse_last_trailer_line_wins():
    body = "Backlog-Closure: x-1111\nsome text\nBacklog-Closure: x-2222 x-3333\n"
    assert parse_closure_trailer(body) == ["x-2222", "x-3333"]


def test_parse_drops_malformed_tokens_on_a_good_line():
    body = "Backlog-Closure: x-5b99 not-an-id x-62a1\n"
    assert parse_closure_trailer(body) == ["x-5b99", "x-62a1"]


def test_parse_empty_or_none_body():
    assert parse_closure_trailer("") == []
    assert parse_closure_trailer(None) == []  # type: ignore[arg-type]


# ---------------------------------------------------------------------------
# render_closure_trailer / render_pr_closure_trailer
# ---------------------------------------------------------------------------


def test_render_round_trips_with_parse():
    line = render_closure_trailer(["x-1234", "x-5678"])
    assert line == "Backlog-Closure: x-1234 x-5678"
    assert parse_closure_trailer(line) == ["x-1234", "x-5678"]


def test_render_empty_for_no_valid_ids():
    assert render_closure_trailer([]) == ""
    assert render_closure_trailer(["not-an-id"]) == ""


def test_contained_descendant_ids():
    entries = [
        _node(id="x-1111"),
        _node(id="x-2222", contained_in="x-1111"),
        _node(id="x-3333", contained_in="x-1111"),
        _node(id="x-4444", contained_in="x-9999"),
    ]
    assert contained_descendant_ids(entries, "x-1111") == ["x-2222", "x-3333"]


def test_render_pr_closure_trailer_target_plus_contained():
    # AC2-HP: a target with two contained descendants -> one trailer, all
    # three ids exactly once.
    entries = [
        _node(id="x-1111"),
        _node(id="x-2222", contained_in="x-1111"),
        _node(id="x-3333", contained_in="x-1111"),
    ]
    line = render_pr_closure_trailer(entries, "x-1111")
    assert line == "Backlog-Closure: x-1111 x-2222 x-3333"


def test_render_pr_closure_trailer_with_extra():
    entries = [_node(id="x-1111")]
    line = render_pr_closure_trailer(entries, "x-1111", extra_ids=["x-9999"])
    assert line == "Backlog-Closure: x-1111 x-9999"


# ---------------------------------------------------------------------------
# bind_closure_claims
# ---------------------------------------------------------------------------


def test_bind_two_nodes_fills_and_appends():
    entries = [
        _node(id="x-1111"),  # no existing PR
        _node(id="x-2222", pr_number=41, pr_url="https://github.com/o/r/pull/41"),
    ]
    result = bind_closure_claims(
        entries,
        ["x-1111", "x-2222"],
        pr_number=42,
        pr_url="https://github.com/o/r/pull/42",
    )
    assert result.outcome == "bound"
    actions = {b.node_id: b.action for b in result.bindings}
    assert actions == {"x-1111": "filled_primary", "x-2222": "appended_additional"}
    n1 = next(e for e in entries if e["id"] == "x-1111")
    assert n1["pr_number"] == 42
    n2 = next(e for e in entries if e["id"] == "x-2222")
    assert n2["pr_number"] == 41  # untouched primary
    assert n2["additional_prs"] == [{"number": 42, "url": "https://github.com/o/r/pull/42"}]


def test_bind_appends_without_corrupting_primary_pair():
    # AC3-EDGE
    entries = [_node(id="x-1111", pr_number=10, pr_url="https://github.com/o/r/pull/10")]
    result = bind_closure_claims(
        entries, ["x-1111"], pr_number=99, pr_url="https://github.com/o/r/pull/99"
    )
    assert result.outcome == "bound"
    n = entries[0]
    assert n["pr_number"] == 10
    assert n["pr_url"] == "https://github.com/o/r/pull/10"
    assert n["additional_prs"] == [{"number": 99, "url": "https://github.com/o/r/pull/99"}]


def test_bind_refuses_unknown_node_mutates_nothing():
    entries = [_node(id="x-1111")]
    before = [dict(e) for e in entries]
    result = bind_closure_claims(
        entries, ["x-1111", "x-9999"], pr_number=1, pr_url="https://github.com/o/r/pull/1"
    )
    assert result.outcome == "refused"
    assert "x-9999" in result.refusal
    assert entries == before  # AC3-ERR: no claimed node mutates


def test_bind_refuses_malformed_claim_mutates_nothing():
    entries = [_node(id="x-1111")]
    before = [dict(e) for e in entries]
    result = bind_closure_claims(
        entries, ["x-1111", "not-an-id"], pr_number=1, pr_url="https://github.com/o/r/pull/1"
    )
    assert result.outcome == "refused"
    assert entries == before


def test_bind_refuses_cross_repo_mutates_nothing():
    entries = [
        _node(id="x-1111"),
        _node(id="x-2222", pr_number=5, pr_url="https://github.com/other/repo/pull/5"),
    ]
    before = [dict(e) for e in entries]
    result = bind_closure_claims(
        entries,
        ["x-1111", "x-2222"],
        pr_number=1,
        pr_url="https://github.com/o/r/pull/1",
    )
    assert result.outcome == "refused"
    assert "cross-repo" in result.refusal
    assert entries == before


def test_bind_refuses_cross_repo_when_existing_ref_has_no_parseable_url():
    # x-59a6 review fix (round 5): the mirror of the cross-repo check above -
    # our_repo IS resolvable this time, but the CLAIMED node's existing ref
    # has pr_number set with no parseable pr_url (a stale pre-repo-scoping
    # stamp). Silently treating that as "no conflict" is the same
    # silent-wrong-close a definite mismatch refuses; fail closed instead.
    entries = [_node(id="x-2222", pr_number=5, pr_url=None)]
    before = [dict(e) for e in entries]
    result = bind_closure_claims(
        entries, ["x-2222"], pr_number=1, pr_url="https://github.com/o/r/pull/1"
    )
    assert result.outcome == "refused"
    assert "unverifiable cross-repo" in result.refusal
    assert entries == before


def test_bind_refuses_unscoped_claim_against_a_node_with_an_existing_ref():
    # x-59a6 review fix: when our_repo cannot be resolved at all (no --repo,
    # unparseable pr_url), the cross-repo check can never run - so a node
    # that already carries SOME PR ref must refuse rather than bind blind.
    entries = [_node(id="x-2222", pr_number=5, pr_url="https://github.com/other/repo/pull/5")]
    before = [dict(e) for e in entries]
    result = bind_closure_claims(
        entries, ["x-2222"], pr_number=1, pr_url="not-a-url", repo=None,
    )
    assert result.outcome == "refused"
    assert "unresolvable" in result.refusal
    assert entries == before


def test_bind_accepts_unscoped_claim_against_a_fresh_node():
    # A node with NO existing ref is still safe to accept when our_repo is
    # unresolvable - there is nothing to collide with.
    entries = [_node(id="x-3333")]
    result = bind_closure_claims(
        entries, ["x-3333"], pr_number=1, pr_url="not-a-url", repo=None,
    )
    assert result.outcome == "bound"
    assert result.bound_ids == ["x-3333"]


def test_bind_is_idempotent_on_rerun():
    # AC4-EDGE: a second reconcile of the same trailer sees the claim, binds
    # zero new refs, and succeeds.
    entries = [_node(id="x-1111"), _node(id="x-2222")]
    pr_url = "https://github.com/o/r/pull/42"
    first = bind_closure_claims(entries, ["x-1111", "x-2222"], pr_number=42, pr_url=pr_url)
    assert set(first.bound_ids) == {"x-1111", "x-2222"}

    second = bind_closure_claims(entries, ["x-1111", "x-2222"], pr_number=42, pr_url=pr_url)
    assert second.outcome == "bound"
    assert second.bound_ids == []
    assert all(b.action == "already_bound" for b in second.bindings)


# ---------------------------------------------------------------------------
# fetch_pr_closure_context
# ---------------------------------------------------------------------------


def test_fetch_closure_context_fails_closed_on_blank_stdout():
    # A gh exit-0 with blank stdout (truncated pipe, a shim that swallowed
    # the verb) must never read as a legitimate empty-body PR - that folded
    # into the json.loads(stdout or "{}") fallback and silently reversed
    # every caller's fail-closed guarantee into fail-open (x-59a6 review fix).
    import subprocess

    def _blank_runner(cmd, **kw):
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    try:
        fetch_pr_closure_context(42, runner=_blank_runner)
    except ClosureQueryError as exc:
        assert "no output" in str(exc)
    else:
        raise AssertionError("expected ClosureQueryError on blank stdout")


def test_bind_skips_already_done_node_without_refusing_batch():
    entries = [
        _node(id="x-1111"),
        _node(id="x-2222", completed_at="2026-08-01T00:00:00Z"),
    ]
    result = bind_closure_claims(
        entries, ["x-1111", "x-2222"], pr_number=7, pr_url="https://github.com/o/r/pull/7"
    )
    assert result.outcome == "bound"
    actions = {b.node_id: b.action for b in result.bindings}
    assert actions["x-1111"] == "filled_primary"
    assert actions["x-2222"] == "already_done"


# ---- Open-PR binding classification (x-d3c6) ----
#
# The pure projection the reconcile heal, `fno do pr list`, and the king board
# all share: one open-PR row -> bound / missing / untracked / ambiguous, with a
# node id only when exactly one real node resolves.

def _open_row(number: int, branch: str, owner: str = "o/r") -> dict:
    return {
        "number": number,
        "url": f"https://github.com/{owner}/pull/{number}",
        "headRefName": branch,
    }


def _classify(rows, entries):
    from fno.graph._reconcile import classify_open_pr_bindings

    return classify_open_pr_bindings(rows, entries)


def test_open_binding_bound_when_the_node_points_back():
    entries = [_node(id="x-1a2b", pr_number=5, pr_url="https://github.com/o/r/pull/5")]
    verdicts = _classify([_open_row(5, "feature/x-1a2b")], entries)
    assert verdicts[0].verdict == "bound"
    assert verdicts[0].node_id == "x-1a2b"


def test_open_binding_bound_via_additional_prs_ref():
    # The negative control for the task-3.1 reporters: a PR already present in
    # the node's additional refs is bound, never reported missing.
    entries = [
        _node(
            id="x-1a2b", pr_number=None,
            additional_prs=[{"number": 5, "url": "https://github.com/o/r/pull/5"}],
        )
    ]
    verdicts = _classify([_open_row(5, "feature/x-1a2b")], entries)
    assert verdicts[0].verdict == "bound"


def test_open_binding_missing_when_the_node_lacks_the_ref():
    entries = [_node(id="x-1a2b", pr_number=None)]
    verdicts = _classify([_open_row(5, "feature/x-1a2b")], entries)
    assert verdicts[0].verdict == "missing"
    assert verdicts[0].node_id == "x-1a2b"


def test_open_binding_untracked_when_the_branch_names_no_real_node():
    entries = [_node(id="x-1a2b")]
    verdicts = _classify([_open_row(5, "chore/tidy-docs")], entries)
    assert verdicts[0].verdict == "untracked"
    assert verdicts[0].node_id is None


def test_open_binding_untracked_on_a_prefix_collision():
    # x-5b66 is a prefix of x-5b667; delimiter-bounded matching must not
    # resolve the shorter id off the longer branch (same rule as the merged
    # reverse map's _branch_matches_node).
    entries = [_node(id="x-5b66")]
    verdicts = _classify([_open_row(5, "feature/x-5b667")], entries)
    assert verdicts[0].verdict == "untracked"


def test_open_binding_ambiguous_when_the_branch_names_several_real_nodes():
    entries = [_node(id="x-1a2b"), _node(id="x-cdef")]
    verdicts = _classify([_open_row(5, "x-1a2b-x-cdef-tally")], entries)
    assert verdicts[0].verdict == "ambiguous"
    assert verdicts[0].node_id is None


def test_open_binding_ambiguous_when_one_node_has_several_open_prs():
    entries = [_node(id="x-1a2b", pr_number=None)]
    verdicts = _classify(
        [_open_row(5, "feature/x-1a2b"), _open_row(6, "target/x-1a2b")], entries
    )
    assert [v.verdict for v in verdicts] == ["ambiguous", "ambiguous"]


def test_open_binding_reads_only_the_branch_never_a_body():
    # AC7: a PR body's prose mentions are invisible here by construction -
    # the classifier's only inputs are the row's headRefName and the graph.
    # Pinned so a future "helpful" body scan cannot creep in silently.
    entries = [_node(id="x-1a2b"), _node(id="x-prose")]
    verdicts = _classify([_open_row(5, "feature/x-1a2b")], entries)
    assert verdicts[0].node_id == "x-1a2b"
    assert all(v.node_id != "x-prose" for v in verdicts)


def test_open_pr_listing_refuses_a_truncated_result():
    from fno.graph._reconcile import ReconcileError, list_open_pr_branches

    calls = []

    def runner(cmd, **kwargs):
        calls.append(cmd)
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps(
                [
                    {
                        "number": number,
                        "url": f"https://github.com/o/r/pull/{number}",
                        "headRefName": f"feature/x-{number:04x}",
                    }
                    for number in range(101)
                ]
            ),
            stderr="",
        )

    with pytest.raises(ReconcileError, match="open PR listing hit its 100-row limit"):
        list_open_pr_branches(cwd="/tmp", runner=runner)
    assert calls[0][calls[0].index("--limit") + 1] == "101"
