"""Unit tests for retro reconcile-findings (phantom-node close sweep).

Drives scan_addressed_findings with injected fetchers so no gh/network is hit.
"""
from __future__ import annotations

from fno.retro.reconcile_findings import retro_targets, scan_addressed_findings


def retro_node(node_id, pr, comment_id, *, completed=False, superseded=False):
    """A backlog node shaped like one retro land writes: a Source permalink + trailer."""
    details = (
        f"Some finding body.\n\n"
        f"Source: PR #{pr}, reviewer `chatgpt-codex-connector[bot]`, "
        f"https://github.com/o/r/pull/{pr}#discussion_r{comment_id}\n\n"
        f"<!-- retro-triage source_pr={pr} finding_hash=deadbeef -->"
    )
    node = {"id": node_id, "details": details}
    if completed:
        node["completed_at"] = "2026-07-22T00:00:00Z"
    if superseded:
        node["superseded_by"] = "ab-other"
    return node


def _fetchers(*, threads_by_pr=None, comments_by_pr=None, commits_by_pr=None, unavailable_prs=()):
    threads_by_pr = threads_by_pr or {}
    comments_by_pr = comments_by_pr or {}
    commits_by_pr = commits_by_pr or {}

    def thread_state(pr, *, repo=None, warnings=None):
        if pr in unavailable_prs:
            return [], True
        return threads_by_pr.get(pr, []), False

    def comments(pr, *, repo=None, warnings=None):
        return comments_by_pr.get(pr, [])

    def commit_dates(pr, *, repo=None, warnings=None):
        return commits_by_pr.get(pr, []), False

    return dict(
        thread_state_fetcher=thread_state,
        comments_fetcher=comments,
        commit_dates_fetcher=commit_dates,
    )


def test_resolved_thread_finding_is_addressed():
    entries = [retro_node("ab-1", 555, "3629390387")]
    fetchers = _fetchers(
        threads_by_pr={555: [{"is_resolved": True, "comment_ids": ["3629390387"]}]}
    )
    found = scan_addressed_findings(entries, **fetchers)
    assert [f.node_id for f in found] == ["ab-1"]
    assert found[0].signal == "resolved/outdated thread"
    assert found[0].pr_number == 555 and found[0].repo == "o/r"


def test_commit_after_bot_finding_is_addressed():
    # The x-632c case: unresolved thread, but a commit landed after the bot comment.
    entries = [retro_node("ab-2", 555, "111")]
    fetchers = _fetchers(
        threads_by_pr={555: [{"is_resolved": False, "is_outdated": False, "comment_ids": ["111"]}]},
        comments_by_pr={
            555: [
                {
                    "id": "111",
                    "body": "![high] guard this",
                    "is_bot": True,
                    "in_reply_to_id": None,
                    "created_at": "2024-01-01T10:00:00Z",
                }
            ]
        },
        commits_by_pr={555: ["2024-01-01T12:00:00+00:00"]},
    )
    found = scan_addressed_findings(entries, **fetchers)
    assert [f.node_id for f in found] == ["ab-2"]
    assert found[0].signal == "commit-after"


def test_unaddressed_finding_is_left_open():
    # Unresolved thread, no commit after -> genuinely open, not swept.
    entries = [retro_node("ab-3", 555, "222")]
    fetchers = _fetchers(
        threads_by_pr={555: [{"is_resolved": False, "is_outdated": False, "comment_ids": ["222"]}]},
        comments_by_pr={
            555: [{"id": "222", "body": "x", "is_bot": True, "in_reply_to_id": None,
                   "created_at": "2024-01-05T10:00:00Z"}]
        },
        commits_by_pr={555: ["2024-01-01T10:00:00+00:00"]},  # commit BEFORE the finding
    )
    assert scan_addressed_findings(entries, **fetchers) == []


def test_unavailable_thread_state_is_skipped_with_warning():
    entries = [retro_node("ab-4", 555, "333")]
    fetchers = _fetchers(unavailable_prs={555})
    warnings: list = []
    found = scan_addressed_findings(entries, warnings=warnings, **fetchers)
    assert found == []  # never close on uncertainty
    assert any("unavailable" in w for w in warnings)


def test_non_retro_and_closed_nodes_are_ignored():
    entries = [
        {"id": "ab-plain", "details": "a hand-filed node, no trailer"},
        retro_node("ab-closed", 555, "444", completed=True),
        retro_node("ab-superseded", 555, "555", superseded=True),
    ]
    # Even though the PR would report these comments addressed, none are open retro nodes.
    fetchers = _fetchers(
        threads_by_pr={555: [{"is_resolved": True, "comment_ids": ["444", "555"]}]}
    )
    assert scan_addressed_findings(entries, **fetchers) == []


def test_designed_node_with_plan_path_is_never_swept():
    # x-3f39/x-cde1 guard: a node someone designed carries real residual work the
    # commit-after heuristic can't see. It must be skipped even when its comment
    # would otherwise read as addressed.
    node = retro_node("ab-designed", 555, "999")
    node["plan_path"] = "internal/fno/plans/2026-07-22-thing-ab-designed.md"
    fetchers = _fetchers(
        threads_by_pr={555: [{"is_resolved": True, "comment_ids": ["999"]}]}
    )
    assert scan_addressed_findings([node], **fetchers) == []
    assert retro_targets([node]) == []


def test_dispatch_scan_can_include_design_bound_node():
    node = retro_node("ab-dispatch", 555, "1000")
    node["plan_path"] = "internal/fno/plans/thing.md"
    fetchers = _fetchers(
        threads_by_pr={555: [{"is_resolved": True, "comment_ids": ["1000"]}]}
    )
    found = scan_addressed_findings([node], include_planned=True, **fetchers)
    assert [finding.node_id for finding in found] == ["ab-dispatch"]


def test_targets_group_by_pr_and_extract_repo():
    entries = [retro_node("ab-a", 555, "1"), retro_node("ab-b", 555, "2"), retro_node("ab-c", 42, "3")]
    targets = retro_targets(entries)
    assert {(t[0], t[1], t[2], t[3]) for t in targets} == {
        ("ab-a", 555, "1", "o/r"),
        ("ab-b", 555, "2", "o/r"),
        ("ab-c", 42, "3", "o/r"),
    }
