"""Characterization tests for the _verify.py port (ab-d4c98550, US2/AC2/AC5).

Mocks gh at the _proc.run seam. Covers verify --kind merged (merge-state
audit, bounded single remediation, record_merge frontmatter write) and
verify --kind reviews (the qualifying-reply gate-flip that closes the
external-review forgery hole).
"""
from __future__ import annotations

import json

import pytest

from fno.config import AutoMergeBlock
from fno.pr import _verify
from fno.pr import _merge
from fno.pr._proc import Result


def _state_file(tmp_path) -> str:
    fno = tmp_path / ".fno"
    fno.mkdir(exist_ok=True)
    sf = fno / "target-state.md"
    sf.write_text('---\nsession_id: "sid-1"\n---\n# state\n')
    return str(sf)


class FakeGH:
    """Dispatch gh/git results by command signature."""

    def __init__(self, *, toplevel, pr_states=None, gh_merge=None, repo="o/r",
                 reviews=None, author="me", issue_comments=None, review_comments=None,
                 checks=None):
        self.toplevel = toplevel
        self.pr_states = list(pr_states or [])
        self.gh_merge = gh_merge or Result(0, "", "")
        self.repo = repo
        self.reviews = reviews if reviews is not None else []
        self.author = author
        self.issue_comments = issue_comments if issue_comments is not None else []
        self.review_comments = review_comments if review_comments is not None else []
        # x-9d11: remediation enforces require_checks_pass in-process, so the
        # default serves a GREEN rollup (state,statusCheckRollup,headRefOid).
        self.checks = checks if checks is not None else {
            "state": "OPEN",
            "headRefOid": "deadbeefcafe",
            "statusCheckRollup": [
                {"name": "c0", "status": "COMPLETED", "conclusion": "SUCCESS"}
            ],
        }
        self.calls = []

    def __call__(self, cmd, *, cwd=None, env=None, input_text=None, timeout=None):
        cmd = list(cmd)
        self.calls.append(cmd)
        if cmd[:3] == ["git", "rev-parse", "--show-toplevel"]:
            return Result(0, self.toplevel + "\n", "")
        if cmd[:3] == ["gh", "pr", "view"] and "state,mergedAt,isDraft,reviewDecision,statusCheckRollup" in cmd:
            nxt = self.pr_states.pop(0) if self.pr_states else {}
            # A None entry means "this read fails" - the gh call itself errors, so
            # _fetch_pr_state returns None and the state is UNKNOWN, distinct from a
            # readable state that is simply not MERGED.
            if nxt is None:
                return Result(1, "", "gh: could not reach api.github.com")
            return Result(0, json.dumps(nxt), "")
        if cmd[:3] == ["gh", "pr", "view"] and any(
            "statusCheckRollup" in a for a in cmd
        ) and "state,mergedAt,isDraft,reviewDecision,statusCheckRollup" not in cmd:
            return Result(0, json.dumps(self.checks) + "\n", "")
        if cmd[:3] == ["gh", "pr", "view"] and "headRefName" in cmd[-1]:
            # The x-9d11 fork guard renders "branch\theadRepo\tbaseRepo" via
            # jq; same-repo by default so the delete fires.
            return Result(0, "feature/x\towner/repo\towner/repo\n", "")
        if cmd[:3] == ["gh", "pr", "merge"]:
            return self.gh_merge
        if cmd[:4] == ["gh", "repo", "view", "--json"]:
            return Result(0, self.repo + "\n", "")
        if cmd[:2] == ["gh", "api"] and "/reviews" in cmd[2]:
            return Result(0, json.dumps(self.reviews), "")
        if cmd[:3] == ["gh", "pr", "view"] and "author" in cmd:
            return Result(0, self.author + "\n", "")
        if cmd[:2] == ["gh", "api"] and "/issues/" in cmd[2]:
            return Result(0, json.dumps(self.issue_comments), "")
        if cmd[:2] == ["gh", "api"] and "/pulls/" in cmd[2] and "/comments" in cmd[2]:
            return Result(0, json.dumps(self.review_comments), "")
        return Result(0, "", "")


@pytest.fixture
def gh_on(monkeypatch):
    monkeypatch.setattr(_verify, "_gh_available", lambda: True)
    monkeypatch.setattr(_verify, "_auto_merge", lambda: AutoMergeBlock(enabled=True))


# ---- verify --kind merged ----


def test_missing_pr_exits_2(tmp_path):
    assert _verify.run_verify_merged("", _state_file(tmp_path)) == 2


def test_unreadable_state_file_exits_2(tmp_path):
    assert _verify.run_verify_merged("42", str(tmp_path / "nope.md")) == 2


def test_gh_missing_degrades_open(tmp_path, monkeypatch):
    monkeypatch.setattr(_verify, "_gh_available", lambda: False)
    assert _verify.run_verify_merged("42", _state_file(tmp_path)) == 0


def test_merged_records_and_exits_0(tmp_path, gh_on, monkeypatch):
    sf = _state_file(tmp_path)
    fake = FakeGH(toplevel=str(tmp_path), pr_states=[{"state": "MERGED", "mergedAt": "2026-06-13T00:00:00Z"}])
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    assert _verify.run_verify_merged("42", sf, cwd=str(tmp_path)) == 0
    content = open(sf).read()
    assert "merged_prs: [42]" in content
    assert 'merged_at: "2026-06-13T00:00:00Z"' in content


def test_closed_blocks_exit_1_and_audits(tmp_path, gh_on, monkeypatch):
    sf = _state_file(tmp_path)
    fake = FakeGH(toplevel=str(tmp_path), pr_states=[{"state": "CLOSED"}])
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    assert _verify.run_verify_merged("42", sf, cwd=str(tmp_path)) == 1
    events = (tmp_path / ".fno" / "events.jsonl").read_text()
    assert "pr_closed_without_merge" in events


def test_draft_blocks_exit_1(tmp_path, gh_on, monkeypatch, capsys):
    sf = _state_file(tmp_path)
    fake = FakeGH(toplevel=str(tmp_path), pr_states=[{"state": "OPEN", "isDraft": True}])
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    assert _verify.run_verify_merged("42", sf, cwd=str(tmp_path)) == 1
    assert "pr_is_draft" in capsys.readouterr().out


def test_changes_requested_blocks_exit_1(tmp_path, gh_on, monkeypatch, capsys):
    sf = _state_file(tmp_path)
    fake = FakeGH(toplevel=str(tmp_path), pr_states=[{"state": "OPEN", "reviewDecision": "CHANGES_REQUESTED"}])
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    assert _verify.run_verify_merged("42", sf, cwd=str(tmp_path)) == 1
    assert "review_changes_requested" in capsys.readouterr().out


def test_failing_required_check_blocks_exit_1(tmp_path, gh_on, monkeypatch, capsys):
    # No isRequired key: gh's statusCheckRollup never emits it (whole-rollup
    # semantics; see _merge._checks_verdict).
    sf = _state_file(tmp_path)
    rollup = [{"name": "ci/build", "conclusion": "FAILURE"}]
    fake = FakeGH(toplevel=str(tmp_path), pr_states=[{"state": "OPEN", "statusCheckRollup": rollup}])
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    assert _verify.run_verify_merged("42", sf, cwd=str(tmp_path)) == 1
    assert "required_checks_failing" in capsys.readouterr().out


def test_pending_check_flows_to_remediation_not_failing(tmp_path, gh_on, monkeypatch, capsys):
    """A still-running check is not-green, not failing: verify must not label
    it required_checks_failing (that would refuse what `fno pr merge` holds
    under a different name, round 10). It flows into remediation reporting."""
    sf = _state_file(tmp_path)
    rollup = [
        {"name": "ci/build", "status": "COMPLETED", "conclusion": "SUCCESS"},
        {"name": "annotate", "status": "IN_PROGRESS", "conclusion": ""},
    ]
    fake = FakeGH(toplevel=str(tmp_path), pr_states=[{"state": "OPEN", "statusCheckRollup": rollup}])
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    assert _verify.run_verify_merged("42", sf, cwd=str(tmp_path)) == 1
    out = capsys.readouterr().out
    assert "required_checks_failing" not in out


def test_remediation_verify_only_blocks_exit_1(tmp_path, monkeypatch, capsys):
    sf = _state_file(tmp_path)
    monkeypatch.setattr(_verify, "_gh_available", lambda: True)
    monkeypatch.setattr(
        _verify, "_auto_merge", lambda: AutoMergeBlock(enabled=True, remediation="verify_only")
    )
    fake = FakeGH(toplevel=str(tmp_path), pr_states=[{"state": "OPEN"}])
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    assert _verify.run_verify_merged("42", sf, cwd=str(tmp_path)) == 1
    assert "remediation_disabled" in capsys.readouterr().out


def test_bounded_remediation_merges_exit_0(tmp_path, gh_on, monkeypatch):
    sf = _state_file(tmp_path)
    fake = FakeGH(
        toplevel=str(tmp_path),
        pr_states=[{"state": "OPEN"}, {"state": "MERGED", "mergedAt": "2026-06-13T01:00:00Z"}],
        gh_merge=Result(0, "", ""),
    )
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    slept = []
    rc = _verify.run_verify_merged("42", sf, cwd=str(tmp_path), sleep_fn=lambda s: slept.append(s))
    assert rc == 0
    # First refetch already MERGED -> no poll.
    assert slept == []
    merge_calls = [c for c in fake.calls if c[:3] == ["gh", "pr", "merge"]]
    assert len(merge_calls) == 1  # single attempt (anti-thrash)


def test_bounded_remediation_stays_single_poll(tmp_path, gh_on, monkeypatch):
    sf = _state_file(tmp_path)
    fake = FakeGH(
        toplevel=str(tmp_path),
        pr_states=[{"state": "OPEN"}, {"state": "OPEN"}, {"state": "OPEN"}],
        gh_merge=Result(0, "", ""),
    )
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    slept = []
    rc = _verify.run_verify_merged("42", sf, cwd=str(tmp_path), sleep_fn=lambda s: slept.append(s))
    assert rc == 1
    assert slept == [30]  # exactly one 30s poll, never a retry loop
    merge_calls = [c for c in fake.calls if c[:3] == ["gh", "pr", "merge"]]
    assert len(merge_calls) == 1


@pytest.mark.parametrize("delete_branch", [True, False])
def test_bounded_remediation_cleanup_split_from_merge(tmp_path, monkeypatch, delete_branch):
    """x-9d11: the remediation merge carries no --delete-branch (gh's local
    delete is the worktree false-failure shape) and no --auto (one arming
    path); when delete_branch_on_merge is set, the REMOTE ref is deleted as a
    warn-only post-merge step, exactly like `fno pr merge`."""
    sf = _state_file(tmp_path)
    monkeypatch.setattr(_verify, "_gh_available", lambda: True)
    monkeypatch.setattr(
        _verify,
        "_auto_merge",
        lambda: AutoMergeBlock(enabled=True, delete_branch_on_merge=delete_branch),
    )
    fake = FakeGH(
        toplevel=str(tmp_path),
        pr_states=[{"state": "OPEN"}, {"state": "MERGED", "mergedAt": "2026-07-28T01:00:00Z"}],
        gh_merge=Result(0, "", ""),
    )
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    assert _verify.run_verify_merged("42", sf, cwd=str(tmp_path), sleep_fn=lambda s: None) == 0
    merge_cmd = [c for c in fake.calls if c[:3] == ["gh", "pr", "merge"]][0]
    assert "--delete-branch" not in merge_cmd, merge_cmd
    assert "--auto" not in merge_cmd, merge_cmd
    assert "--match-head-commit" in merge_cmd, merge_cmd
    remote_deletes = [
        c for c in fake.calls if "DELETE" in c and "/git/refs/heads/" in c[-1]
    ]
    assert (len(remote_deletes) == 1) is delete_branch
    if delete_branch:
        # Against the PR's verified base repo, never `git push origin`.
        assert remote_deletes[0][-1] == "repos/owner/repo/git/refs/heads/feature/x"


def test_bounded_remediation_worktree_delete_error_records_merge(tmp_path, gh_on, monkeypatch, capsys):
    """The worktree-recovery branch must catch git's DELETE phrasing, not only the
    checkout phrasing. ``gh pr merge --delete-branch`` lands the merge then fails
    the local delete because a worktree holds the branch; the PR is MERGED, so
    verify must record it and return 0. Matching only ``already used by worktree``
    (checkout) left this case to fall through to ``merge_attempt_failed``,
    unrecorded - so the /target gate that calls verify re-verified forever (the
    same defect on the sibling path)."""
    sf = _state_file(tmp_path)
    fake = FakeGH(
        toplevel=str(tmp_path),
        pr_states=[{"state": "OPEN"}, {"state": "MERGED", "mergedAt": "2026-08-06T05:54:59Z"}],
        gh_merge=Result(
            1,
            "",
            "failed to delete local branch feature/x-beb7: failed to run git: "
            "error: cannot delete branch 'feature/x-beb7' used by worktree at "
            "'/repo/.claude/worktrees/x-beb7'",
        ),
    )
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    rc = _verify.run_verify_merged("42", sf, cwd=str(tmp_path), sleep_fn=lambda s: None)
    assert rc == 0
    assert "verify-pr-merged" in capsys.readouterr().out


def test_unreadable_state_after_merge_error_is_substrate_failure(tmp_path, gh_on, monkeypatch, capsys):
    """Sibling of the _merge.py guard. The always-re-read fix depends on the
    re-read succeeding; when `_fetch_pr_state` returns None the merge state is
    UNKNOWN, not "not merged". The `(pr_json or {})` idiom flattens those two
    apart-cases together, so an unreadable state would report merge_attempt_failed
    and the /target gate would treat a possibly-landed merge as broken. Report the
    substrate failure (exit 2) instead, so the caller retries the read."""
    sf = _state_file(tmp_path)
    fake = FakeGH(
        toplevel=str(tmp_path),
        pr_states=[{"state": "OPEN"}, None],
        gh_merge=Result(1, "", "failed to delete remote branch: remote: error: internal"),
    )
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    rc = _verify.run_verify_merged("42", sf, cwd=str(tmp_path), sleep_fn=lambda s: None)
    assert rc == 2
    out = capsys.readouterr().out
    assert "merge_state_unreadable" in out
    assert "merge_attempt_failed" not in out


def test_unknown_state_degrades_open(tmp_path, gh_on, monkeypatch):
    sf = _state_file(tmp_path)
    fake = FakeGH(toplevel=str(tmp_path), pr_states=[{"state": "WEIRD"}])
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    assert _verify.run_verify_merged("42", sf, cwd=str(tmp_path)) == 0


# ---- verify --kind reviews ----


def test_reviews_no_reviewers_exit_0(tmp_path, monkeypatch):
    sf = _state_file(tmp_path)
    monkeypatch.setattr(_verify, "_gh_available", lambda: True)
    fake = FakeGH(toplevel=str(tmp_path), reviews=[])
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    assert _verify.run_verify_reviews("42", sf, cwd=str(tmp_path)) == 0


def test_reviews_qualifying_reply_within_24h_exit_0(tmp_path, monkeypatch):
    sf = _state_file(tmp_path)
    monkeypatch.setattr(_verify, "_gh_available", lambda: True)
    fake = FakeGH(
        toplevel=str(tmp_path),
        reviews=[{"login": "bot", "submitted_at": "2026-06-13T00:00:00Z"}],
        author="me",
        issue_comments=[{"login": "me", "created_at": "2026-06-13T01:00:00Z", "body": "fixed"}],
    )
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    assert _verify.run_verify_reviews("42", sf, cwd=str(tmp_path)) == 0


def test_reviews_no_qualifying_reply_flips_exit_1(tmp_path, monkeypatch, capsys):
    sf = _state_file(tmp_path)
    monkeypatch.setattr(_verify, "_gh_available", lambda: True)
    fake = FakeGH(
        toplevel=str(tmp_path),
        reviews=[{"login": "bot", "submitted_at": "2026-06-13T00:00:00Z"}],
        author="me",
        issue_comments=[],  # no reply at all
    )
    monkeypatch.setattr(_verify, "run", fake)
    monkeypatch.setattr(_merge, "run", fake)
    rc = _verify.run_verify_reviews("42", sf, cwd=str(tmp_path))
    assert rc == 1
    out = capsys.readouterr().out
    assert "flipped back to false" in out and "bot" in out
    events = (tmp_path / ".fno" / "events.jsonl").read_text()
    audit = json.loads(events.strip().splitlines()[-1])
    assert audit["data"]["gate"] == "external_review_passed"
    assert audit["data"]["reviewer"] == "bot"


# ---- qualifying-reply predicate (the forgery-hole-closing logic) ----


def test_predicate_reply_before_review_does_not_qualify():
    comments = [{"login": "me", "created_at": "2026-06-12T00:00:00Z", "body": "@bot"}]
    assert not _verify._has_qualifying_reply(comments, "bot", "2026-06-13T00:00:00Z", "me")


def test_predicate_mention_after_review_qualifies_even_past_24h():
    comments = [{"login": "me", "created_at": "2026-06-20T00:00:00Z", "body": "@bot done"}]
    assert _verify._has_qualifying_reply(comments, "bot", "2026-06-13T00:00:00Z", "me")


def test_predicate_non_author_reply_does_not_qualify():
    comments = [{"login": "someone", "created_at": "2026-06-13T01:00:00Z", "body": "@bot"}]
    assert not _verify._has_qualifying_reply(comments, "bot", "2026-06-13T00:00:00Z", "me")
