"""Characterization tests for the _merge.py port (ab-d4c98550, US1/AC1/AC5).

Mocks gh/git at the _proc.run seam so the guard-rejection, classification,
and worktree-recovery branches are exercised deterministically (they are hard
to reproduce against a live PR). Pins the JSON-line schema, the exit codes,
and the stdout-vs-stderr routing the bash used.
"""
from __future__ import annotations

import json

import pytest

from fno.config import AutoMergeBlock
from fno.pr import _coverage_gate, _merge
from fno.pr._proc import Result, ToolMissing


class FakeRun:
    """Dispatch canned Results by command, recording every call."""

    def __init__(
        self,
        *,
        gh_merge: Result | None = None,
        merged_at: str = "null",
        view_url: str = "https://example/pr",
        api_ok: bool = False,
        toplevel: str | None = None,
        behind_by: int = 0,
        view_fails: bool = False,
        auto_merge_request: str = "null",
        head_ref: str = "feature/x",
        head_repo: str = "owner/repo",
        base_repo: str = "owner/repo",
        checks: dict | None = None,
    ) -> None:
        self.gh_merge = gh_merge or Result(0, "", "")
        self.view_fails = view_fails
        self.merged_at = merged_at
        self.view_url = view_url
        self.api_ok = api_ok
        self.toplevel = toplevel
        self.behind_by = behind_by
        self.auto_merge_request = auto_merge_request
        self.head_ref = head_ref
        self.head_repo = head_repo
        self.base_repo = base_repo
        # require_checks_pass is enforced in-process now (x-9d11), so the
        # default fake serves a GREEN rollup; tests exercising refusal paths
        # pass their own.
        self.checks = checks if checks is not None else {
            "state": "OPEN",
            "headRefOid": "deadbeefcafe",
            "statusCheckRollup": [
                {"name": "c0", "status": "COMPLETED", "conclusion": "SUCCESS"}
            ],
        }
        self.calls: list[list[str]] = []

    def __call__(self, cmd, *, cwd=None, env=None, input_text=None, timeout=None):
        cmd = list(cmd)
        self.calls.append(cmd)
        tool = cmd[0]
        if tool == "git":
            if cmd[1:3] == ["rev-parse", "--show-toplevel"]:
                return Result(0, (self.toplevel or cwd or "") + "\n", "")
            if cmd[1:4] == ["remote", "get-url", "origin"]:
                return Result(0, "git@github.com:owner/repo.git\n", "")
            return Result(0, "", "")
        if tool == "gh":
            if cmd[1:3] == ["pr", "merge"]:
                return self.gh_merge
            if cmd[1:3] == ["pr", "view"]:
                if any("statusCheckRollup" in a for a in cmd):
                    return Result(0, json.dumps(self.checks) + "\n", "")
                if cmd[-1] == "headRefOid":
                    # The coverage gate's head fetch: a merge first pins the
                    # head coverage would describe. Served from the same
                    # default the rollup read carries, so a test faking a
                    # moved head overrides `checks` and both reads agree.
                    return Result(
                        0,
                        json.dumps({"headRefOid": self.checks.get("headRefOid", "deadbeefcafe")}) + "\n",
                        "",
                    )
                if "mergedAt" in cmd:
                    if self.view_fails:
                        return Result(1, "", "gh: could not reach api.github.com")
                    return Result(0, self.merged_at + "\n", "")
                if "autoMergeRequest" in cmd:
                    # Models `-q .autoMergeRequest.enabled`: "true" armed,
                    # "false"/"null" not (jq prints null, not empty).
                    return Result(0, self.auto_merge_request + "\n", "")
                if "headRefName" in cmd[-1] and "headRepository" in cmd[-1]:
                    # x-9d11 fork guard: jq renders "branch\theadRepo\tbaseRepo"
                    # (tab separators; an empty head repo = deleted fork).
                    return Result(
                        0,
                        f"{self.head_ref}\t{self.head_repo}\t{self.base_repo}\n",
                        "",
                    )
                if cmd[-1] == ".headRefName":
                    return Result(0, self.head_ref + "\n", "")
                if "baseRefName,headRefName" in cmd:
                    return Result(
                        0,
                        json.dumps({"baseRefName": "main", "headRefName": self.head_ref}) + "\n",
                        "",
                    )
                return Result(0, self.view_url + "\n", "")
            if cmd[1] == "api":
                endpoint = cmd[-1]
                if endpoint.startswith("repos/owner/repo/pulls/") and "/comments" not in endpoint:
                    return Result(
                        0,
                        json.dumps(
                            {
                                "number": int(endpoint.rsplit("/", 1)[-1]),
                                "state": str(self.checks.get("state") or "OPEN").lower(),
                                "merged": self.checks.get("state") == "MERGED",
                                "mergeable": True,
                                "head": {
                                    "sha": self.checks.get("headRefOid", "deadbeefcafe"),
                                    "ref": self.head_ref,
                                },
                                "base": {"ref": "main"},
                            }
                        )
                        + "\n",
                        "",
                    )
                if "/check-runs?" in endpoint:
                    rows = []
                    for check in self.checks.get("statusCheckRollup") or []:
                        if not check.get("name"):
                            continue
                        rows.append(
                            {
                                "name": check.get("name"),
                                "status": str(check.get("status") or "completed").lower(),
                                "conclusion": str(check.get("conclusion") or "").lower(),
                                "started_at": check.get("startedAt") or "2026-08-18T00:00:00Z",
                            }
                        )
                    return Result(
                        0,
                        json.dumps({"total_count": len(rows), "check_runs": rows}) + "\n",
                        "",
                    )
                if endpoint.endswith("/status"):
                    rows = [
                        {
                            "context": check.get("context"),
                            "state": check.get("state"),
                            "created_at": check.get("createdAt") or "2026-08-18T00:00:00Z",
                        }
                        for check in self.checks.get("statusCheckRollup") or []
                        if check.get("context")
                    ]
                    return Result(0, json.dumps({"statuses": rows}) + "\n", "")
                if len(cmd) > 2 and "/compare/" in cmd[2]:
                    return Result(0, f"{self.behind_by}\n", "")
                if "DELETE" in cmd and "/git/refs/heads/" in cmd[-1]:
                    # The post-merge branch delete (gh api against the verified
                    # base repo, never `git push origin`).
                    return Result(0, "", "")
                return Result(0, "", "") if self.api_ok else Result(1, "", "api failed")
        if tool == "bash":
            return Result(0, "", "")
        return Result(0, "", "")


@pytest.fixture
def enabled(monkeypatch, tmp_path):
    monkeypatch.setattr(_merge, "_load_auto_merge", lambda: AutoMergeBlock(enabled=True))
    monkeypatch.setattr(_merge.shutil, "which", lambda _x: "/usr/bin/gh")
    # Hermetic merge-lock: route the LD#9 serialization claim (and the lane
    # probe) to a tmp claims root so tests never touch the repo's .fno/claims
    # or contend with a real in-flight merge.
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    # x-0eaf: default to a covered review so existing merge-behavior tests
    # proceed past the coverage guard. No head_sha -> covered_head is empty, so
    # the --match-head-commit pin (and the head-pin tests' own verified_head)
    # are undisturbed; the pin is exercised by its own test below.
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: ({"coverage": "covered", "reviewed_count": 1}, ""),
    )
    monkeypatch.setattr(_merge, "_review_lane_configured", lambda repo, pr_number=0: True)
    # No 3am valve by default. The gate reads the override label on every
    # verdict, and an unstubbed read is a real `gh pr view` per merge case.
    monkeypatch.setattr(
        "fno.pr._reviews._override_label_actor", lambda pr, repo, r: (False, None)
    )
    # The merge's server receipt is best-effort; hermetic tests stub the
    # publisher so no real gh spawn rides along with every merge case.
    monkeypatch.setattr(
        "fno.pr._reviews.publish_coverage_status",
        lambda pr, head=None, cwd=None, repo=None, gate_verdict=None: (True, ""),
    )
    # Hermetic hold-check: point graph_json at a file that does not exist so
    # hold_for_pr's by_id is always empty and it returns None without a live
    # `gh pr view --json ...body...` fetch. Session-scoped HOME sandboxing
    # (conftest.py) is per-worker, not per-test, so an unpinned graph_json
    # here would pick up nodes another test in this worker wrote earlier -
    # FakeRun's generic `gh pr view` fallback answers a bare URL, not JSON,
    # so a fetch that should never fire (this PR names no held node) instead
    # trips hold_for_pr's fail-closed path (round-11 review fix exposed
    # this).
    monkeypatch.setattr("fno.paths.graph_json", lambda: tmp_path / "graph.json")


def _last_json(capsys, *, stream="out") -> dict:
    cap = capsys.readouterr()
    text = cap.out if stream == "out" else cap.err
    return json.loads(text.strip().splitlines()[-1])


# ---- arg validation ----


def test_unknown_arg_exits_1(capsys):
    assert _merge.run_merge(["--bogus"]) == 1


def test_missing_pr_exits_1(capsys):
    assert _merge.run_merge([]) == 1


def test_legacy_invoker_flag_is_accepted_not_rejected(monkeypatch, capsys, tmp_path):
    # x-04ab removed --invoker; a lingering legacy flag is silently accepted
    # (never an error). The merge proceeds and is gated only by `enabled`, so
    # with auto-merge off it skips (exit 2) exactly as a no-flag call would.
    monkeypatch.setattr(_merge, "_load_auto_merge", lambda: AutoMergeBlock(enabled=False))
    # Same hermeticity as the `enabled` fixture: without the pin a populated
    # per-worker sandbox graph turns this into `held` before `enabled` runs.
    monkeypatch.setattr("fno.paths.graph_json", lambda: tmp_path / "graph.json")
    assert _merge.run_merge(["--invoker=anything", "42"]) == 2
    assert _last_json(capsys)["outcome"] == "skipped"


def test_invalid_pr_number_exits_1_with_failed_json_on_stderr(capsys):
    assert _merge.run_merge(["0"]) == 1
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "failed"
    assert "invalid pr number" in obj["reason"]


def test_plan_dispatch_hold_refuses_sanctioned_merge(monkeypatch, capsys, tmp_path):
    monkeypatch.setattr(
        "fno.pr._hold.merge_hold_reason",
        lambda pr, cwd: "dispatch-hold:x-5a5c: blocking finding; set_by=king",
    )
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys)
    assert obj["outcome"] == "held"
    assert "set_by=king" in obj["reason"]


# ---- config + gh gates ----


def test_auto_merge_disabled_skips_exit_2(monkeypatch, capsys, tmp_path):
    monkeypatch.setattr(_merge, "_load_auto_merge", lambda: AutoMergeBlock(enabled=False))
    # Without the pin this pays a live `gh pr view` against the populated
    # per-worker sandbox graph (network luck, not hermeticity).
    monkeypatch.setattr("fno.paths.graph_json", lambda: tmp_path / "graph.json")
    assert _merge.run_merge(["42"]) == 2
    obj = _last_json(capsys)
    assert obj["outcome"] == "skipped"
    assert obj["pr"] == 42


def test_gh_missing_exits_127(monkeypatch, capsys, tmp_path):
    monkeypatch.setattr(_merge, "_load_auto_merge", lambda: AutoMergeBlock(enabled=True))
    # Same hermeticity as the `enabled` fixture: a populated per-worker
    # sandbox graph makes the hold lookup's closure fetch fail-closed (exit 2)
    # before the gh-missing check this test exists to pin ever runs.
    monkeypatch.setattr("fno.paths.graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setattr(_merge.shutil, "which", lambda _x: None)
    # Isolate from an ambient target-state.md: run_merge with no cwd reads
    # `auto_merge_approved` from the caller's repo state, so a suite run inside
    # an active /target worktree (whose manifest carries a per-run no-merge)
    # bails on that field before the gh check. A fresh tmp cwd has no state.
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 127
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "failed"
    assert obj["reason"] == "gh CLI not installed"


# ---- classification ----


def test_merge_immediate_exit_0(enabled, monkeypatch, capsys, tmp_path):
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    obj = _last_json(capsys)
    assert obj["outcome"] == "merged"
    assert obj["strategy"] == "merge"
    assert "invoker" not in obj


def test_fence_crash_failopen_emits_gate_escape(enabled, monkeypatch, capsys, tmp_path):
    """F4: a fence-CODE crash fails open (merge proceeds) but must not read as a
    clean merge - a gate_escape is emitted so retro/audit see the skipped fence."""
    _write_manifest(tmp_path, "session_id: s1\nauto_merge_approved: true\n")
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    def boom(uuid, **k):
        raise RuntimeError("fence boom")

    monkeypatch.setattr("fno.claims.incarnation.incarnation_fence_blocks", boom)
    emitted = []
    monkeypatch.setattr(
        "fno.events.gate_escape.emit_gate_escape",
        lambda *a, **k: emitted.append((a, k)),
    )

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0  # fail-open proceeded
    assert _last_json(capsys)["outcome"] == "merged"
    assert emitted, "a fence-crash fail-open must emit gate_escape"
    args, kwargs = emitted[0]
    assert args[0] == "other"
    assert "incarnation-fence" in (kwargs.get("detail") or "")
    assert kwargs.get("pr") == 42


def _write_manifest(tmp_path, body: str) -> None:
    (tmp_path / ".fno").mkdir(exist_ok=True)
    (tmp_path / ".fno" / "target-state.md").write_text(body, encoding="utf-8")


def test_per_run_no_merge_skips_even_when_config_enabled(
    enabled, monkeypatch, capsys, tmp_path
):
    """`auto_merge.enabled` is policy; the manifest carries THIS run's decision.

    A per-run `no-merge` (which `/target bg` injects by default) resolves to
    `auto_merge_approved: false` while `enabled` stays true. Gating on config
    alone made the sanctioned verb a weaker gate than raw `gh pr merge`, which
    the git-protection hook already guards on this same field.
    """
    _write_manifest(tmp_path, "session_id: s1\nauto_merge_enabled: true\nauto_merge_approved: false\n")
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys)
    assert obj["outcome"] == "skipped"
    assert "no-merge" in obj["reason"]


def test_manifest_refusal_names_the_source(enabled, monkeypatch, capsys, tmp_path):
    """x-9d11: the refusal names WHICH input set the posture, so the operator's
    first question ("what layer said no") is answered by the receipt itself."""
    _write_manifest(
        tmp_path,
        "session_id: s1\nauto_merge_enabled: true\nauto_merge_approved: false\n"
        "auto_merge_source: flag-no-merge\n",
    )
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys)
    assert obj["outcome"] == "skipped"
    assert "auto_merge_source: flag-no-merge" in obj["reason"]


def test_manifest_refusal_without_source_reads_unknown(enabled, monkeypatch, capsys, tmp_path):
    """AC4-ERR: a pre-provenance manifest carries no source; the refusal reads
    `unknown`, never a guessed origin."""
    _write_manifest(tmp_path, "session_id: s1\nauto_merge_enabled: true\nauto_merge_approved: false\n")
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys)
    assert obj["outcome"] == "skipped"
    assert "auto_merge_source: unknown" in obj["reason"]


def test_manifest_approved_true_still_merges(enabled, monkeypatch, capsys, tmp_path):
    _write_manifest(tmp_path, "session_id: s1\nauto_merge_approved: true\n")
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"


def test_manifest_without_the_field_merges(enabled, monkeypatch, capsys, tmp_path):
    """Absent field -> proceed. A manual `fno pr merge` outside a target session,
    or against a pre-field manifest, must not start refusing."""
    _write_manifest(tmp_path, "session_id: s1\n")
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"


def test_merge_exit_0_with_queue_text_still_merged(enabled, monkeypatch, capsys, tmp_path):
    """x-9d11: the verb never queues, so gh output text cannot reclassify the
    outcome. A green exit is `merged` whatever the message says (the queued
    outcome is gone with --auto; arming the queue is finalize's job)."""
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(
        gh_merge=Result(0, "Pull request #42 will be automatically merged", ""),
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"


def test_merge_failed_protected_exit_1(enabled, monkeypatch, capsys, tmp_path):
    fake = FakeRun(gh_merge=Result(1, "", "branch is protected"))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 1
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "failed"
    assert obj["reason"] == "branch protected"


def test_worktree_recovery_already_merged_serverside(enabled, monkeypatch, capsys, tmp_path):
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(
        gh_merge=Result(1, "", "fatal: 'main' is already used by worktree at /x"),
        merged_at="2026-06-13T00:00:00Z",
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    obj = _last_json(capsys)
    assert obj["outcome"] == "merged"
    assert "server-side" in obj["reason"]


def test_worktree_recovery_api_fallback(enabled, monkeypatch, capsys, tmp_path):
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(
        gh_merge=Result(1, "", "is already used by worktree"),
        merged_at="null",
        api_ok=True,
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    obj = _last_json(capsys)
    assert obj["outcome"] == "merged"
    assert "worktree fallback" in obj["reason"]
    # The API path uses a literal that does NOT contain "gh pr merge".
    api_calls = [c for c in fake.calls if c[:2] == ["gh", "api"] and "PUT" in c]
    assert api_calls


def test_worktree_branch_delete_failure_reports_merged(enabled, monkeypatch, capsys, tmp_path):
    """Primary specimen (PR #742). ``gh pr merge --delete-branch`` exits non-zero
    when the post-merge local branch delete fails because a worktree holds the
    branch, even though the server-side merge landed. git's delete error
    ("cannot delete branch ... used by worktree") is NOT the checkout-refused
    phrasing the recovery block matches, so it falls through to the post-merge
    guard: outcome=merged with cleanup=failed and the error visible in reason
    (the step ran and failed - it was not skipped). A landed merge must report
    merged, and the cleanup failure must stay visible."""
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(
        gh_merge=Result(
            1,
            "",
            "failed to delete local branch feature/x-beb7: failed to run git: "
            "error: cannot delete branch 'feature/x-beb7' used by worktree at "
            "'/repo/.claude/worktrees/x-beb7'",
        ),
        merged_at="2026-08-06T05:54:59Z",
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["742"], cwd=str(tmp_path)) == 0
    obj = _last_json(capsys)
    assert obj["outcome"] == "merged"
    assert obj["cleanup"] == "failed"
    assert "cleanup failed" in obj["reason"]
    assert "cannot delete branch" in obj["reason"]


def test_base_branch_held_by_worktree_reports_merged_with_cleanup(enabled, monkeypatch, capsys, tmp_path):
    """The base-branch-checkout failure is also a post-merge cleanup failure, not
    a 'skipped' step: gh merges server-side, then cannot switch to main because
    the canonical worktree holds it ('main is already used by worktree'). The
    always-re-read guard reports merged + cleanup=failed with the error visible,
    whatever git's phrasing - this is the checkout-phrasing sibling of the delete
    specimen, and both must surface the cleanup result."""
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(
        gh_merge=Result(1, "", "fatal: 'main' is already used by worktree at '/repo'"),
        merged_at="2026-08-06T05:54:59Z",
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["742"], cwd=str(tmp_path)) == 0
    obj = _last_json(capsys)
    assert obj["outcome"] == "merged"
    assert obj["cleanup"] == "failed"
    assert "cleanup failed" in obj["reason"]


def test_post_merge_cleanup_failure_never_reports_failed(enabled, monkeypatch, capsys, tmp_path):
    """General invariant. A post-merge cleanup failure whose error is NOT the
    worktree phrasing (here a remote branch delete) must still not report failed
    when the merge landed. The fallthrough re-reads mergedAt and, because the
    merge landed, reports merged with cleanup=failed - the cleanup result visible
    in its own field, never swallowed, never the merge's outcome."""
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(
        gh_merge=Result(
            1,
            "",
            "failed to delete remote branch: remote: error: internal",
        ),
        merged_at="2026-08-06T05:54:59Z",
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["742"], cwd=str(tmp_path)) == 0
    obj = _last_json(capsys)
    assert obj["outcome"] == "merged"
    assert obj["cleanup"] == "failed"
    assert "cleanup failed" in obj["reason"]


def test_unreadable_merge_state_holds_never_reports_failed(enabled, monkeypatch, capsys, tmp_path):
    """The landed-merge guard depends on reading the PR's merged state. When that
    read ITSELF fails, the merge state is unknown - not "not merged". Reporting
    `failed` there asserts merge truth we do not have, and an autonomous caller
    keying on `outcome` retries a merge that may already have landed. The guard
    reports `held` (exit 2, retry-later) so the uncertainty stays in the receipt."""
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(
        gh_merge=Result(1, "", "failed to delete remote branch: remote: error: internal"),
        view_fails=True,
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["742"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys)
    assert obj["outcome"] == "held"
    assert obj["outcome"] != "failed"
    assert "unreadable" in obj["reason"]
    # The gh error is surfaced, not swallowed - the receipt names why it is unknown.
    assert "api.github.com" in obj["reason"]


def test_readable_not_merged_still_reports_failed(enabled, monkeypatch, capsys, tmp_path):
    """Control for the guard above: a READABLE state that says not-merged must
    still report failed. The held path is strictly for an unreadable state, so a
    genuine merge failure is never softened into a retry."""
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(
        gh_merge=Result(1, "", "Pull request is not mergeable"),
        merged_at="null",
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["742"], cwd=str(tmp_path)) == 1
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "failed"


# ---- post-merge followups ----


def test_post_merge_sentinels_written(enabled, monkeypatch, tmp_path):
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(gh_merge=Result(0, "Merged", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    _merge.run_merge(["42"], cwd=str(tmp_path))
    mem = tmp_path / ".fno" / ".memory-pass-pending"
    triage = tmp_path / ".fno" / ".triage-pending"
    assert mem.read_text().strip() == "42"
    sentinel = json.loads(triage.read_text())
    assert sentinel["pr_number"] == 42
    assert sentinel["mode"] == "interactive"
    assert sentinel["pr_url"] == "https://example/pr"


def test_post_merge_mode_autonomous_with_megawalk_state(enabled, monkeypatch, tmp_path):
    fno_dir = tmp_path / ".fno"
    fno_dir.mkdir()
    (fno_dir / "megawalk-state.md").write_text("x\n")
    fake = FakeRun(gh_merge=Result(0, "Merged", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    _merge.run_merge(["7"], cwd=str(tmp_path))
    sentinel = json.loads((fno_dir / ".triage-pending").read_text())
    assert sentinel["mode"] == "autonomous"


def test_session_satisfied_emitted_when_state_present(enabled, monkeypatch, tmp_path):
    fno_dir = tmp_path / ".fno"
    fno_dir.mkdir()
    (fno_dir / "target-state.md").write_text(
        '---\nsession_id: "20260613T000000Z-1-abc"\n---\n'
    )
    fake = FakeRun(gh_merge=Result(0, "Merged", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    _merge.run_merge(["42"], cwd=str(tmp_path))
    events = fno_dir / "events.jsonl"
    assert events.exists()
    line = json.loads(events.read_text().strip().splitlines()[-1])
    assert line["type"] == "session_satisfied"
    assert line["data"]["source"] == "pr_merge"
    assert line["data"]["session_id"] == "20260613T000000Z-1-abc"


# ---- stub-manifest draft-held guard (G3, x-24b7) ----


def _held(*_a, **_k):
    return {"_node": "x-9", "stubs": [{"stub_id": "a"}]}


def test_unreconciled_stub_manifest_holds_merge_exit_2(enabled, monkeypatch, capsys, tmp_path):
    # AC3-ERR / AC7-EDGE: auto_merge ENABLED, but a contract dependent's
    # unreconciled manifest still refuses the merge, and the merge subcommand is
    # never invoked (no mocks ship).
    import fno.stub_manifest as sm
    monkeypatch.setattr(sm, "unreconciled_manifest_for_pr", _held)
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys)
    assert obj["outcome"] == "held"
    assert "x-9" in obj["reason"]
    assert not any(c[1:3] == ["pr", "merge"] for c in fake.calls)


def test_hard_node_merges_unaffected_by_guard(enabled, monkeypatch, capsys, tmp_path):
    # AC6-EDGE: guard returns None for a non-contract PR -> normal merge.
    (tmp_path / ".fno").mkdir()
    import fno.stub_manifest as sm
    monkeypatch.setattr(sm, "unreconciled_manifest_for_pr", lambda *a, **k: None)
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"


def test_guard_own_failure_does_not_block_merge(enabled, monkeypatch, capsys, tmp_path):
    # The guard is best-effort: if its own lookup raises, a normal merge proceeds.
    (tmp_path / ".fno").mkdir()
    import fno.stub_manifest as sm

    def _boom(*_a, **_k):
        raise RuntimeError("graph wedged")

    monkeypatch.setattr(sm, "unreconciled_manifest_for_pr", _boom)
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"


# ---- merge serialization + stale-base hold (parallel mode G4, LD#9) ----


def _lock_key():
    from fno.paths import resolve_canonical_repo_root

    return f"merge:{resolve_canonical_repo_root()}"


def test_merge_lock_held_by_peer_exits_2_held(enabled, monkeypatch, capsys, tmp_path):
    from fno.claims.core import acquire_claim

    acquire_claim(_lock_key(), "pr-merge:peer", reason="test peer merge")
    monkeypatch.setattr(_merge, "_MERGE_LOCK_WAIT_S", 0)
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys)
    assert obj["outcome"] == "held"
    assert "merge serialized" in obj["reason"]
    # the gh merge was never attempted while a peer holds the lock
    assert not any(c[1:3] == ["pr", "merge"] for c in fake.calls)


def test_merge_lock_released_after_merge(enabled, monkeypatch, tmp_path, capsys):
    from fno.claims.core import acquire_claim

    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    # a new holder can take the lock immediately -> it was released, not leaked
    acquire_claim(_lock_key(), "pr-merge:next", reason="post-release probe")


def test_merge_lock_unavailable_fails_open(enabled, monkeypatch, capsys, tmp_path):
    import fno.paths as paths

    monkeypatch.setattr("fno.pr._hold.merge_hold_reason", lambda pr, cwd: None)

    def _boom():
        raise RuntimeError("no canonical root")

    monkeypatch.setattr(paths, "resolve_canonical_repo_root", _boom)
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"


def test_stale_base_with_live_lanes_holds_exit_2(enabled, monkeypatch, capsys, tmp_path):
    monkeypatch.setattr(_merge, "_live_lane_count", lambda: 1)
    fake = FakeRun(
        gh_merge=Result(0, "Merged pull request", ""),
        toplevel=str(tmp_path),
        behind_by=3,
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys)
    assert obj["outcome"] == "held"
    assert "stale base" in obj["reason"]
    assert "fno pr rebase" in obj["reason"]
    assert not any(c[1:3] == ["pr", "merge"] for c in fake.calls)


def test_stale_base_ignored_when_no_lanes(enabled, monkeypatch, capsys, tmp_path):
    # Sequential path (no live lanes): behind-ness is never consulted and the
    # merge proceeds exactly as before parallel mode existed.
    monkeypatch.setattr(_merge, "_live_lane_count", lambda: 0)
    fake = FakeRun(
        gh_merge=Result(0, "Merged pull request", ""),
        toplevel=str(tmp_path),
        behind_by=3,
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"
    assert not any(len(c) > 2 and c[1] == "api" and "/compare/" in c[2] for c in fake.calls)


def _point_lane_read_at(monkeypatch, **fields):
    # _review_lane_configured imports load_settings_for_repo at call time, so
    # patching the module attribute is seen by the local import.
    from types import SimpleNamespace

    import fno.config as cfg

    review_fields = dict(
        required_bots=None,
        optional_apps=None,
        reviewers=None,
        peers=[],
        peer_identity=None,
    )
    review_fields.update(fields)
    review = SimpleNamespace(**review_fields)
    monkeypatch.setattr(
        cfg, "load_settings_for_repo", lambda _path: SimpleNamespace(review=review)
    )


@pytest.mark.parametrize(
    "fields,expected",
    [
        # Stock install: no lane, so the coverage guard does not apply.
        (dict(), False),
        # peers present but peer_identity set: peers post under the shared
        # login, not a distinct local lane. Rust agrees (loopcheck.rs:2492).
        (dict(peers=["codex"], peer_identity="fno-peer-bot"), False),
        # Every peer carries its own identity: GitHub logins, not a local lane.
        (dict(peers=[{"provider": "openai", "identity": "codex-bot"}]), False),
        # An identity-free peer (bare string) is a local-attestation lane.
        (dict(peers=["codex"]), True),
        # An identity-free peer (dict without identity) is a local-attestation lane.
        (dict(peers=[{"provider": "openai"}]), True),
        # An explicit reviewer lane.
        (dict(reviewers=["code-review"]), True),
    ],
)
def test_review_lane_configured_matches_loopcheck_gate(
    monkeypatch, fields, expected, tmp_path
):
    _point_lane_read_at(monkeypatch, **fields)
    assert _merge._review_lane_configured(str(tmp_path)) is expected


def test_review_lane_configured_resolves_toplevel_from_a_subdirectory(
    monkeypatch, tmp_path
):
    """A lane declared at the project root is still seen from a subdirectory.

    load_settings_for_repo reads ``<arg>/.fno/`` with NO upward walk, so passing
    the raw invocation directory made ``fno pr merge`` run from ``cli/`` report
    "no lane" - which short-circuits ``covered`` to True and lets an unreviewed
    PR merge. The guard was decorative on that path.

    The parametrized test above cannot catch this: it monkeypatches
    load_settings_for_repo, so it pins the predicate while stubbing out the path
    resolution the bug lives in. This one uses the REAL loader against real
    files, and isolates the global layer via FNO_GLOBAL_SETTINGS_PATH
    (``/dev/null`` is the documented disable hook) - without that the
    developer's own ~/.fno lane leaks in and masks the failure entirely.
    """
    import subprocess

    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")
    repo = tmp_path / "repo"
    (repo / ".fno").mkdir(parents=True)
    (repo / "sub").mkdir()
    subprocess.run(["git", "init", "-q", str(repo)], check=True)
    # The lane exists ONLY at the project root.
    (repo / ".fno" / "config.toml").write_text(
        '[review]\ngithub_apps = ["chatgpt-codex-connector"]\n'
    )

    assert _merge._review_lane_configured(str(repo)) is True
    assert _merge._review_lane_configured(str(repo / "sub")) is True


def test_review_lane_configured_floors_a_code_payload(monkeypatch, tmp_path):
    """A code payload on a lane-less stock install requires review (the
    self-review floor), so the merge coverage guard engages for the same PR the
    stop gate holds. Mirrors floor_self_review in loopcheck.rs."""
    from fno.harness_identity import HARNESS_SESSION_MARKERS

    for marker, _harness in HARNESS_SESSION_MARKERS:
        monkeypatch.delenv(marker, raising=False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "claude-test-session")
    # Stock install: no lane. A code payload + pr_number -> floored to True.
    _point_lane_read_at(monkeypatch)
    monkeypatch.setattr(_merge, "_pr_payload_is_code", lambda repo, pr_number: True)
    assert _merge._review_lane_configured(str(tmp_path), 42) is True
    # Opt-out: self_review_required=false restores unreviewed code-PR shipping.
    _point_lane_read_at(monkeypatch, self_review_required=False)
    assert _merge._review_lane_configured(str(tmp_path), 42) is False
    # A docs payload -> no floor.
    _point_lane_read_at(monkeypatch)
    monkeypatch.setattr(_merge, "_pr_payload_is_code", lambda repo, pr_number: False)
    assert _merge._review_lane_configured(str(tmp_path), 42) is False


def test_stock_code_floor_skips_a_harness_without_a_self_review_verb(
    monkeypatch, tmp_path
):
    """Route 3 is deferred, so Gemini must not be given an unsatisfiable floor."""
    from fno.harness_identity import HARNESS_SESSION_MARKERS

    for marker, _harness in HARNESS_SESSION_MARKERS:
        monkeypatch.delenv(marker, raising=False)
    monkeypatch.setenv("GEMINI_SESSION_ID", "gemini-test-session")
    _point_lane_read_at(monkeypatch)
    monkeypatch.setattr(_merge, "_pr_payload_is_code", lambda repo, pr_number: True)

    assert _merge._review_lane_configured(str(tmp_path), 42) is False
    assert _merge._code_review_attestation_required(str(tmp_path), 42) is False


def test_stock_code_floor_does_not_guess_between_mixed_harness_markers(
    monkeypatch, tmp_path
):
    """Match Rust: inherited markers from two harness families are ambiguous."""
    from fno.harness_identity import HARNESS_SESSION_MARKERS

    for marker, _harness in HARNESS_SESSION_MARKERS:
        monkeypatch.delenv(marker, raising=False)
    monkeypatch.setenv("CODEX_THREAD_ID", "foreign-codex-thread")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "claude-session")
    _point_lane_read_at(monkeypatch)
    monkeypatch.setattr(_merge, "_pr_payload_is_code", lambda repo, pr_number: True)

    assert _merge._review_lane_configured(str(tmp_path), 42) is False
    assert _merge._code_review_attestation_required(str(tmp_path), 42) is False


def test_pr_payload_classifier_is_documentation_aware_and_fail_closed(monkeypatch):
    """The merge-side classifier matches loopcheck's notion of documentation and
    fails closed on a missing gh (a degraded probe must not bypass the guard)."""
    assert _merge._is_documentation_path("docs/a.md") is True
    assert _merge._is_documentation_path("README.md") is True
    assert _merge._is_documentation_path("src/lib.rs") is False
    assert _merge._is_documentation_path(".fno/config.toml") is False
    monkeypatch.setattr(_merge.shutil, "which", lambda _x: None)
    assert _merge._pr_payload_is_code("/nope", 42) is True


def test_up_to_date_head_with_live_lanes_merges(enabled, monkeypatch, capsys, tmp_path):
    monkeypatch.setattr(_merge, "_live_lane_count", lambda: 1)
    fake = FakeRun(
        gh_merge=Result(0, "Merged pull request", ""),
        toplevel=str(tmp_path),
        behind_by=0,
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"


def test_merge_lock_released_when_merge_body_raises(enabled, monkeypatch, tmp_path, capsys):
    # Regression pin: an exception thrown through the lock's yield must still
    # release the claim (the original except-then-yield shape leaked it).
    from fno.claims.core import acquire_claim

    def _boom(*_a, **_k):
        raise RuntimeError("merge body exploded")

    monkeypatch.setattr(_merge, "_do_merge", _boom)
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    with pytest.raises(RuntimeError, match="merge body exploded"):
        _merge.run_merge(["42"], cwd=str(tmp_path))
    # the lock was released on the way out, not leaked
    acquire_claim(_lock_key(), "pr-merge:next", reason="post-raise probe")


# ---------------------------------------------------------------------------
# Repo without auto-merge enabled (x-3218 side-fix)
# ---------------------------------------------------------------------------


def _rollup(*conclusions, head="deadbeefcafe"):
    return {
        "state": "OPEN",
        "headRefOid": head,
        "statusCheckRollup": [
            {"name": f"c{i}", "status": "COMPLETED", "conclusion": c}
            for i, c in enumerate(conclusions)
        ],
    }


class _AutoMergeRejectingRun(FakeRun):
    """Serves a rollup (default green) and models a racing head at merge time.

    Name predates x-9d11, when gh rejected ``--auto``; the verb no longer sends
    it, so this fake now exists for the checks-verdict fixtures and the
    head-moved discrimination below."""

    def __init__(self, *, rollup=None, head_moved=False, live_head="pushed9999", **kw):
        super().__init__(**kw)
        self.merge_cmds: list[list[str]] = []
        self.rollup = rollup if rollup is not None else _rollup("SUCCESS", "SUCCESS")
        self.checks = self.rollup
        self.head_moved = head_moved
        # The head as it exists server-side at merge time. head_moved=True means
        # someone pushed after the rollup read, so this differs from the rollup's.
        self.live_head = live_head

    def __call__(self, cmd, *, cwd=None, env=None, input_text=None, timeout=None):
        cmd = list(cmd)
        if cmd[:3] == ["gh", "pr", "merge"]:
            self.merge_cmds.append(cmd)
            if "--auto" in cmd:
                return Result(
                    1,
                    "",
                    "GraphQL: Auto merge is not allowed for this repository "
                    "(enablePullRequestAutoMerge)",
                )
            if self.head_moved:
                # Model gh's ACTUAL behaviour, so the test discriminates: the
                # refusal happens only because a pin was sent and no longer
                # matches. An unpinned merge succeeds - which is exactly the
                # bug, so deleting the pin must turn this test red.
                if "--match-head-commit" not in cmd:
                    return Result(0, "Merged pull request #42", "")
                sent = cmd[cmd.index("--match-head-commit") + 1]
                if sent != self.live_head:
                    return Result(
                        1,
                        "",
                        "Head branch was modified. Review and try the merge again.",
                    )
            return Result(0, "Merged pull request #42", "")
        if cmd[:3] == ["gh", "pr", "view"] and any("statusCheckRollup" in a for a in cmd):
            return Result(0, json.dumps(self.rollup) + "\n", "")
        return super().__call__(
            cmd, cwd=cwd, env=env, input_text=input_text, timeout=timeout
        )


def _checks_enabled(monkeypatch):
    monkeypatch.setattr(
        _merge,
        "_load_auto_merge",
        lambda: AutoMergeBlock(enabled=True, require_checks_pass=True),
    )


def test_checks_verdict_can_ignore_only_the_coverage_status(monkeypatch):
    rollup = {
        "headRefOid": "abc123",
        "statusCheckRollup": [
            {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {
                "context": "fno/review-coverage",
                "state": "FAILURE",
                "createdAt": "2026-08-18T01:00:00Z",
            },
        ],
    }
    monkeypatch.setattr(
        "fno.pr._rest.fetch_pr_rest",
        lambda pr, cwd=None, runner=None: (rollup, ""),
    )

    assert _merge._checks_verdict(42, "/repo")[0] == "red"
    verdict, counts, head = _merge._checks_verdict(
        42, "/repo", ignore_contexts=("fno/review-coverage",)
    )
    assert verdict == "green"
    assert counts["total"] == 1
    assert head == "abc123"


def test_checks_verdict_keeps_other_failing_contexts(monkeypatch):
    rollup = {
        "headRefOid": "abc123",
        "statusCheckRollup": [
            {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"context": "fno/review-coverage", "state": "FAILURE"},
            {"context": "security/policy", "state": "FAILURE"},
        ],
    }
    monkeypatch.setattr(
        "fno.pr._rest.fetch_pr_rest",
        lambda pr, cwd=None, runner=None: (rollup, ""),
    )

    verdict, counts, _head = _merge._checks_verdict(
        42, "/repo", ignore_contexts=("fno/review-coverage",)
    )
    assert verdict == "red"
    assert counts["fail"] == 1


def test_green_checks_merge_in_one_call_without_auto(
    enabled, monkeypatch, capsys, tmp_path
):
    """x-9d11: no queue lane. require_checks_pass is enforced in-process (read
    the checks, merge only on green), so ONE merge call, never ``--auto`` -
    and a repo without the auto-merge feature merges an already-green PR the
    same as one with it (x-8543)."""
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    fake = _AutoMergeRejectingRun(toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    obj = _last_json(capsys)
    assert obj["outcome"] == "merged"
    assert obj["reason"] == "merged immediately"

    assert len(fake.merge_cmds) == 1
    assert "--auto" not in fake.merge_cmds[0]


def test_the_merge_is_pinned_to_the_head_the_verdict_was_read_from(
    enabled, monkeypatch, capsys, tmp_path
):
    """Nothing re-checks at merge time, so the SHA is pinned to the verdict.

    A verdict belongs to one commit; a push landing between the read and the
    merge would otherwise slip an unverified head through (codex P1 on #623).
    """
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    fake = _AutoMergeRejectingRun(
        rollup=_rollup("SUCCESS", head="abc123def456"), toplevel=str(tmp_path)
    )
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    merge = fake.merge_cmds[0]
    assert "--match-head-commit" in merge
    assert merge[merge.index("--match-head-commit") + 1] == "abc123def456"


def test_a_racing_push_makes_the_pinned_retry_refuse(
    enabled, monkeypatch, capsys, tmp_path
):
    """The pin's whole purpose: a moved head fails instead of merging.

    The fake refuses ONLY a stale pin and merges an unpinned request, so
    deleting the pin flips this test red rather than leaving it green on a
    failure it did not cause.
    """
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    fake = _AutoMergeRejectingRun(head_moved=True, toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 1
    assert _last_json(capsys, stream="err")["outcome"] == "failed"


def test_a_green_verdict_without_a_readable_head_refuses(
    enabled, monkeypatch, capsys, tmp_path
):
    """Green but unpinnable is not mergeable - fail closed rather than unpinned."""
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    fake = _AutoMergeRejectingRun(
        rollup=_rollup("SUCCESS", head=""), toplevel=str(tmp_path)
    )
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    blocked = _last_json(capsys, stream="err")
    assert blocked["outcome"] == "blocked"
    assert "head" in blocked["reason"]
    # Refused BEFORE any merge call: the pin is a precondition, not a retry.
    assert len(fake.merge_cmds) == 0


def test_a_red_pr_is_refused_before_any_merge_call(
    enabled, monkeypatch, capsys, tmp_path
):
    """The load-bearing case: enforcing the invariant in-process must not lose
    the red guard the queue used to provide."""
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    fake = _AutoMergeRejectingRun(
        rollup=_rollup("SUCCESS", "FAILURE"), toplevel=str(tmp_path)
    )
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 1
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "failed"
    assert "red" in obj["reason"]
    assert len(fake.merge_cmds) == 0


def test_auto_merge_unsupported_repo_holds_on_pending_checks(
    enabled, monkeypatch, capsys, tmp_path
):
    """Pending is a hold, not a failure: the PR is still merge-eligible later."""
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    rollup = {
        "state": "OPEN",
        "headRefOid": "deadbeefcafe",
        "statusCheckRollup": [
            {"name": "a", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"name": "b", "status": "IN_PROGRESS", "conclusion": ""},
        ],
    }
    fake = _AutoMergeRejectingRun(rollup=rollup, toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    assert _last_json(capsys)["outcome"] == "held"
    assert len(fake.merge_cmds) == 0


def test_auto_merge_unsupported_repo_holds_on_an_unreadable_rollup(
    enabled, monkeypatch, capsys, tmp_path
):
    """No verdict is not a green light, and not a failed ship either: an
    empty rollup (gh failure, or a repo with no CI) is retry-later. A repo
    with no checks configured needs require_checks_pass=false, not a red
    node status (round 7)."""
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    fake = _AutoMergeRejectingRun(rollup={}, toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    held = _last_json(capsys)
    assert "unknown" in held["reason"]
    assert held["outcome"] == "held"
    assert len(fake.merge_cmds) == 0


def test_a_genuine_merge_failure_is_not_retried_without_auto(
    enabled, monkeypatch, capsys, tmp_path
):
    """Only the capability refusal retries; a real failure stands as-is."""
    _checks_enabled(monkeypatch)
    fake = FakeRun(gh_merge=Result(1, "", "branch is protected"), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 1
    assert _last_json(capsys, stream="err")["reason"] == "branch protected"
    assert sum(1 for c in fake.calls if c[:3] == ["gh", "pr", "merge"]) == 1


class _WorktreeFallbackRun(_AutoMergeRejectingRun):
    """`--auto` rejected, then the pinned retry hits the worktree checkout error.

    That drops into the server-side REST recovery, which is where the pin was
    previously lost.
    """

    def __init__(self, **kw):
        super().__init__(**kw)
        self.api_cmds: list[list[str]] = []

    def __call__(self, cmd, *, cwd=None, env=None, input_text=None, timeout=None):
        cmd = list(cmd)
        if cmd[:3] == ["gh", "pr", "merge"]:
            self.merge_cmds.append(cmd)
            if "--auto" in cmd:
                return Result(
                    1,
                    "",
                    "GraphQL: Auto merge is not allowed for this repository "
                    "(enablePullRequestAutoMerge)",
                )
            return Result(1, "", "fatal: 'feature/x' is already used by worktree at ...")
        if cmd[:2] == ["gh", "api"] and any("/merge" in a for a in cmd):
            self.api_cmds.append(cmd)
            return Result(0, '{"merged":true}', "")
        if cmd[:3] == ["gh", "pr", "view"] and "mergedAt" in " ".join(cmd):
            return Result(0, "null\n", "")
        if cmd[:3] == ["gh", "pr", "view"] and any("statusCheckRollup" in a for a in cmd):
            return Result(0, json.dumps(self.rollup) + "\n", "")
        return FakeRun.__call__(
            self, cmd, cwd=cwd, env=env, input_text=input_text, timeout=timeout
        )


def test_worktree_server_side_recovery_carries_the_head_pin(
    enabled, monkeypatch, capsys, tmp_path
):
    """The pin must survive into the REST fallback, not just the gh retry.

    A worktree run is the COMMON path here, and the REST merge would otherwise
    merge whatever the head is now - silently undoing the `--match-head-commit`
    guard in exactly the case that reaches it.
    """
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    fake = _WorktreeFallbackRun(
        rollup=_rollup("SUCCESS", head="feed1234beef"), toplevel=str(tmp_path)
    )
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"
    assert len(fake.api_cmds) == 1
    joined = " ".join(fake.api_cmds[0])
    assert "sha=feed1234beef" in joined, joined


def test_worktree_recovery_without_a_verified_head_sends_no_pin(
    enabled, monkeypatch, capsys, tmp_path
):
    """When --auto was never used, this process vouched for nothing.

    require_checks_pass off means the caller opted out of the CI gate entirely;
    inventing a pin here would change behavior on a path this PR does not own.
    """
    (tmp_path / ".fno").mkdir()
    monkeypatch.setattr(
        _merge,
        "_load_auto_merge",
        lambda: AutoMergeBlock(enabled=True, require_checks_pass=False),
    )
    fake = _WorktreeFallbackRun(toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert len(fake.api_cmds) == 1
    assert "sha=" not in " ".join(fake.api_cmds[0])


# ── x-9d11: cleanup split from the merge; one arming path ───────────────────


def test_merge_never_passes_delete_branch_and_deletes_remote_after(
    enabled, monkeypatch, capsys, tmp_path
):
    """AC3-HP: with delete_branch_on_merge=true, the gh call carries no
    --delete-branch (no local git ops, so a worktree-held branch cannot fail
    the merge), and the REMOTE ref is deleted afterwards as cleanup."""
    (tmp_path / ".fno").mkdir()
    monkeypatch.setattr(
        _merge,
        "_load_auto_merge",
        lambda: AutoMergeBlock(enabled=True, delete_branch_on_merge=True),
    )
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    obj = _last_json(capsys)
    assert obj["outcome"] == "merged"
    assert obj.get("cleanup", "") == ""
    merge_calls = [c for c in fake.calls if c[:3] == ["gh", "pr", "merge"]]
    assert len(merge_calls) == 1
    assert "--delete-branch" not in merge_calls[0]
    remote_deletes = [
        c for c in fake.calls
        if "DELETE" in c and c[-1].endswith("/git/refs/heads/feature/x")
    ]
    assert len(remote_deletes) == 1, fake.calls
    # The ref path names the PR's verified base repo (owner/repo by default).
    assert remote_deletes[0][-1] == "repos/owner/repo/git/refs/heads/feature/x"


def test_remote_delete_failure_cannot_fail_the_merge(
    enabled, monkeypatch, capsys, tmp_path
):
    """A remote-branch delete that fails (protected, already gone) warns in the
    cleanup field and never retracts the landed merge."""
    (tmp_path / ".fno").mkdir()
    monkeypatch.setattr(
        _merge,
        "_load_auto_merge",
        lambda: AutoMergeBlock(enabled=True, delete_branch_on_merge=True),
    )

    class _NoDelete(FakeRun):
        def __call__(self, cmd, **kw):
            if cmd[:2] == ["gh", "api"] and "DELETE" in cmd:
                return Result(1, "", "remote ref protected")
            return super().__call__(cmd, **kw)

    fake = _NoDelete(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    obj = _last_json(capsys)
    assert obj["outcome"] == "merged"
    assert obj.get("cleanup", "").startswith("failed")


def test_fork_pr_skips_remote_delete(
    enabled, monkeypatch, capsys, tmp_path
):
    """A fork PR's head branch lives on the fork, not this repo's origin: the
    delete must never fire (it would destroy an unrelated same-named branch on
    the base repo)."""
    (tmp_path / ".fno").mkdir()
    monkeypatch.setattr(
        _merge,
        "_load_auto_merge",
        lambda: AutoMergeBlock(enabled=True, delete_branch_on_merge=True),
    )
    fake = FakeRun(
        gh_merge=Result(0, "Merged pull request", ""),
        head_repo="contributor/fork",
        base_repo="owner/repo",
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    obj = _last_json(capsys)
    assert obj["outcome"] == "merged"
    assert obj.get("cleanup", "") == ""
    assert not [c for c in fake.calls if c[:2] == ["gh", "api"] and "DELETE" in c]


def test_deleted_fork_head_repo_skips_remote_delete(
    enabled, monkeypatch, capsys, tmp_path
):
    """A null headRepository (deleted fork repo) renders as an empty field:
    the guard must still skip - never fall through to a delete against the
    base repo's origin."""
    (tmp_path / ".fno").mkdir()
    monkeypatch.setattr(
        _merge,
        "_load_auto_merge",
        lambda: AutoMergeBlock(enabled=True, delete_branch_on_merge=True),
    )
    fake = FakeRun(
        gh_merge=Result(0, "Merged pull request", ""),
        head_repo="",
        base_repo="owner/repo",
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys).get("cleanup", "") == ""
    assert not [c for c in fake.calls if c[:2] == ["gh", "api"] and "DELETE" in c]


def test_an_already_armed_pr_skips_naming_finalize(
    enabled, monkeypatch, capsys, tmp_path
):
    """AC5-CON: finalize owns GitHub's auto-merge queue; a PR already armed
    there makes this verb stand down rather than race it."""
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(
        gh_merge=Result(0, "Merged pull request", ""),
        auto_merge_request="true",
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys)
    assert obj["outcome"] == "skipped"
    assert "finalize" in obj["reason"]
    assert not any(c[:3] == ["gh", "pr", "merge"] for c in fake.calls)


def test_a_degraded_checks_read_names_why_it_could_not_tell(
    enabled, monkeypatch, capsys, tmp_path
):
    """"unknown" alone cannot distinguish a broken gh from a PR with no checks.

    The operator only ever sees the emitted reason, so the miss carries a why -
    the same shape `_behind_by` already uses for its probe misses.
    """
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)

    class _BadRollup(_AutoMergeRejectingRun):
        def __call__(self, cmd, *, cwd=None, env=None, input_text=None, timeout=None):
            cmd = list(cmd)
            if cmd[:2] == ["gh", "api"] and any("/check-runs?" in a for a in cmd):
                return Result(0, "not json at all", "")
            return super().__call__(
                cmd, cwd=cwd, env=env, input_text=input_text, timeout=timeout
            )

    fake = _BadRollup(toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    # held, not failed (round 7): an unreadable rollup is retry-later, never a
    # failed-ship stamp on the node.
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    held = _last_json(capsys)
    assert held["outcome"] == "held"
    assert "not JSON" in held["reason"], held["reason"]


def test_a_missing_gh_during_the_checks_read_keeps_exit_127(
    enabled, monkeypatch, capsys, tmp_path
):
    """The module reserves 127 for a missing gh; the checks read must not
    demote that to a generic exit-1 "checks are unknown"."""
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)

    class _GhVanishes(_AutoMergeRejectingRun):
        def __call__(self, cmd, *, cwd=None, env=None, input_text=None, timeout=None):
            cmd = list(cmd)
            if cmd[:2] == ["gh", "api"] and any("/check-runs?" in a for a in cmd):
                raise ToolMissing("gh")
            return super().__call__(
                cmd, cwd=cwd, env=env, input_text=input_text, timeout=timeout
            )

    fake = _GhVanishes(toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 127
    assert _last_json(capsys, stream="err")["reason"] == "gh CLI not installed"


def test_a_missing_gh_during_the_already_armed_probe_keeps_exit_127(
    enabled, monkeypatch, capsys, tmp_path
):
    """_already_armed's own gh call owes the same 127 contract its sibling
    checks/merge calls have (review round 12): it must not propagate a raw
    ToolMissing past _do_merge."""
    (tmp_path / ".fno").mkdir()

    class _GhVanishes(_AutoMergeRejectingRun):
        def __call__(self, cmd, *, cwd=None, env=None, input_text=None, timeout=None):
            cmd = list(cmd)
            if cmd[:3] == ["gh", "pr", "view"] and "autoMergeRequest" in cmd:
                raise ToolMissing("gh")
            return super().__call__(
                cmd, cwd=cwd, env=env, input_text=input_text, timeout=timeout
            )

    fake = _GhVanishes(toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 127
    assert _last_json(capsys, stream="err")["reason"] == "gh CLI not installed"


def test_a_red_refusal_marks_the_node_failed_not_still_queued(
    enabled, monkeypatch, capsys, tmp_path
):
    """A node left `queued` by an earlier attempt must not read queued after a
    red refusal - the scoreboard consumes that field."""
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    seen: list[str] = []
    monkeypatch.setattr(
        _merge, "_sync_graph_merge_status", lambda status, pr, cwd="": seen.append(status)
    )
    fake = _AutoMergeRejectingRun(
        rollup=_rollup("FAILURE"), toplevel=str(tmp_path)
    )
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 1
    assert seen == ["failed"], seen


def test_a_pending_hold_does_not_mark_the_node_failed(
    enabled, monkeypatch, capsys, tmp_path
):
    """Held is not failed: the PR is still merge-eligible once checks finish."""
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    seen: list[str] = []
    monkeypatch.setattr(
        _merge, "_sync_graph_merge_status", lambda status, pr, cwd="": seen.append(status)
    )
    rollup = {
        "state": "OPEN",
        "headRefOid": "deadbeefcafe",
        "statusCheckRollup": [{"name": "a", "status": "IN_PROGRESS", "conclusion": ""}],
    }
    fake = _AutoMergeRejectingRun(rollup=rollup, toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    assert seen == [], seen


# ---- coverage guard (x-0eaf) ----


def test_coverage_override_is_not_vetoed_by_its_stale_failure_status(
    enabled, monkeypatch, capsys, tmp_path
):
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    monkeypatch.setattr(
        "fno.pr._reviews._override_label_actor",
        lambda pr, repo, runner: (True, "maintainer"),
    )
    rollup = {
        "state": "OPEN",
        "headRefOid": "deadbeefcafe",
        "statusCheckRollup": [
            {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"context": "fno/review-coverage", "state": "FAILURE"},
        ],
    }
    fake = _AutoMergeRejectingRun(rollup=rollup, toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"
    assert len(fake.merge_cmds) == 1


def test_coverage_missing_refuses(enabled, monkeypatch, capsys, tmp_path):
    """No review_coverage event -> Unknown -> the sanctioned merge refuses."""
    # Pin the head the stubbed (absent) row would describe: the gate refuses a
    # head it could not fetch, and this test pins the missing-ROW refusal.
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: "abc")
    monkeypatch.setattr(_merge, "_review_coverage_for_pr", lambda pr, repo, head=None: (None, ""))
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "blocked"
    assert "unreviewed" in obj["reason"]


def test_coverage_zero_refuses(enabled, monkeypatch, capsys, tmp_path):
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: ({"coverage": "covered", "reviewed_count": 0, "head_sha": "abc"}, ""),
    )
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    assert _last_json(capsys, stream="err")["outcome"] == "blocked"


def test_coverage_unknown_refuses(enabled, monkeypatch, capsys, tmp_path):
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: ({"coverage": "unknown", "head_sha": "abc"}, ""),
    )
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    assert _last_json(capsys, stream="err")["outcome"] == "blocked"


def test_coverage_covered_proceeds(enabled, monkeypatch, capsys, tmp_path):
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: ({"coverage": "covered", "reviewed_count": 1, "head_sha": "abc"}, ""),
    )
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: "abc")
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"


def test_covered_merge_publishes_coverage_status(enabled, monkeypatch, capsys, tmp_path):
    """A covered merge leaves the server-visible receipt behind.

    The commit status the repo ruleset requires is written from the same
    predicate that satisfied the gate here, so the sanctioned path can never be
    the one missing the green marker GitHub judges merges by. The receipt is
    pinned to the covered head: the sha the verdict describes, and the sha the
    merge itself is pinned to.
    """
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: ({"coverage": "covered", "reviewed_count": 1, "head_sha": "abc"}, ""),
    )
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: "abc")
    monkeypatch.setattr(
        _merge, "_code_review_attestation_required", lambda repo, pr_number=0: False
    )
    published = []
    monkeypatch.setattr(
        "fno.pr._reviews.publish_coverage_status",
        lambda pr, head=None, cwd=None, repo=None, gate_verdict=None: published.append(
            (pr, head, gate_verdict)
        )
        or (True, ""),
    )
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"
    assert published and published[0][:2] == (42, "abc"), (
        "covered merge must publish the status on the covered head"
    )
    verdict = published[0][2]
    assert verdict is not None and verdict[0] == _coverage_gate.COVERED, (
        "the receipt must carry the gate's own verdict tuple, never a fresh "
        "read that may have flipped between the gate and the stamp"
    )


def test_fresh_eval_carrying_only_a_stale_bot_verdict_refuses(
    enabled, monkeypatch, capsys, tmp_path
):
    """x-5b99: the exact hole, at the gate that was supposed to catch it.

    The staleness check below the coverage guard compares the EVAL head against
    the current PR head, and it works. It just answers a different question: a
    coverage event computed thirty seconds ago is fresh by that measure while
    the only verdict inside it came from a bot that read a commit two commits
    back. A fresh eval carrying a stale bot verdict walked straight through.

    What closes it is upstream (loop-check no longer counts a stale verdict, so
    the count arrives at zero and the word arrives as "uncovered"), which is why
    the merge logic here is UNCHANGED. This test pins that the shape now
    refuses, so a later change that restores the old serialization cannot
    quietly reopen the door.
    """
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: ({
            "coverage": "uncovered",
            "reviewed_count": 0,
            # Fresh eval: this IS the PR's current head, so the staleness check
            # has nothing to object to.
            "head_sha": "89bc0b91",
            "verdicts": [
                {
                    "producer": "github_app",
                    "name": "chatgpt-codex-connector",
                    "verdict": "stale",
                    "reviewed_sha": "8e557ccd",
                    "freshness": "stale",
                }
            ],
        }, ""),
    )
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: "89bc0b91")
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "blocked"
    # The refusal names coverage, not staleness: the eval was not stale.
    assert "uncovered" in obj["reason"]


def test_carried_local_attestation_still_satisfies_the_code_review_gate(
    enabled, monkeypatch, capsys, tmp_path
):
    """x-62a1: a verdict carried across a rebase is a `reviewed` verdict here.

    The merge gate reads the verdict enum, not the freshness, so the carry has
    to arrive as `reviewed` for the required code-review entry to stay
    satisfied. If a future change records a carry as its own verdict value
    instead, this fails loudly rather than silently demanding a re-review at
    the one moment losing an attestation costs most.
    """
    state = tmp_path / ".fno"
    state.mkdir()
    (state / "config.toml").write_text(
        '[review]\nreviewers = ["code-review"]\n', encoding="utf-8"
    )
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: ({
            "coverage": "covered",
            "reviewed_count": 1,
            "head_sha": "abc",
            "verdicts": [
                {
                    "producer": "local_attestation",
                    "name": "code-review",
                    "verdict": "reviewed",
                    "reviewed_sha": "oldhead",
                    "freshness": "carried_base_sync",
                }
            ],
        }, ""),
    )
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: "abc")
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"


def test_code_review_gate_rejects_unrelated_github_app_coverage(
    enabled, monkeypatch, capsys, tmp_path
):
    """A required local code-review cannot be replaced by an unrelated App review."""
    state = tmp_path / ".fno"
    state.mkdir()
    (state / "config.toml").write_text(
        '[review]\nreviewers = ["code-review"]\n', encoding="utf-8"
    )
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: ({
            "coverage": "covered",
            "reviewed_count": 1,
            "head_sha": "abc",
            "verdicts": [
                {
                    "name": "chatgpt-codex-connector",
                    "producer": "github_app",
                    "verdict": "reviewed",
                }
            ],
        }, ""),
    )

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "blocked"
    assert "code-review" in obj["reason"]
    assert not any(c[:3] == ["gh", "pr", "merge"] for c in fake.calls)


def test_code_review_gate_accepts_its_head_pinned_local_attestation(
    enabled, monkeypatch, capsys, tmp_path
):
    state = tmp_path / ".fno"
    state.mkdir()
    (state / "config.toml").write_text(
        '[review]\nreviewers = ["code-review"]\n', encoding="utf-8"
    )
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: ({
            "coverage": "covered",
            "reviewed_count": 1,
            "head_sha": "abc",
            "verdicts": [
                {
                    "name": "code-review",
                    "producer": "local_attestation",
                    "verdict": "reviewed",
                }
            ],
        }, ""),
    )
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: "abc")

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"


def test_coverage_stale_head_refuses(enabled, monkeypatch, capsys, tmp_path):
    """Coverage pinned a head that no longer matches the PR head -> stale -> refuse."""
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: ({"coverage": "covered", "reviewed_count": 2, "head_sha": "oldhead"}, ""),
    )
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: "newhead")
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    assert _last_json(capsys, stream="err")["outcome"] == "blocked"


def test_covered_head_pins_the_merge_cmd(monkeypatch, tmp_path):
    """x-0eaf: the covered head pins the merge so a racing push cannot land an
    unreviewed head (x-9d11: there is no --auto queue to pin anymore; the
    covered head outranks the checks-verdict head on the one merge call)."""
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    monkeypatch.setattr(_merge.shutil, "which", lambda _x: "/usr/bin/gh")
    # Same hermeticity as the `enabled` fixture: the closure fetch a populated
    # per-worker sandbox graph triggers answers non-JSON and holds the merge
    # before any merge command is recorded.
    monkeypatch.setattr("fno.paths.graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: ({"coverage": "covered", "reviewed_count": 1, "head_sha": "coveredSHA"}, ""),
    )
    monkeypatch.setattr(_merge, "_review_lane_configured", lambda repo, pr_number=0: True)
    # Fresh coverage: the live PR head IS the covered head, so the gate passes
    # and the covered head reaches the merge command.
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: "coveredSHA")
    fake = _AutoMergeRejectingRun(
        rollup=_rollup("SUCCESS", head="coveredSHA"), toplevel=str(tmp_path)
    )
    monkeypatch.setattr(_merge, "run", fake)
    _merge.run_merge(["42"], cwd=str(tmp_path))
    merge_cmd = fake.merge_cmds[0]
    assert "--auto" not in merge_cmd, "the verb never queues (x-9d11)"
    i = merge_cmd.index("--match-head-commit")
    assert merge_cmd[i + 1] == "coveredSHA"


# ── refusal reasons name the cause ───────────────────────────────────────────
#
# A refusal that reports only a count leaves the reader to invent the cause, and
# the nearest exclusion-flavoured vocabulary in the repo is `attestation_origin`,
# which gates nothing. Two workers escalated green, unblocked PRs to the operator
# on exactly that inference; both were looking at a head that had moved. These
# pin the cause into the message so the vacuum is not there to fill.


def test_stale_head_refusal_names_both_shas_and_the_action():
    """The staleness branch flips `covered` False in run_merge, but the reason
    function knew nothing about it and reported the count instead. Its output
    for a moved head was the self-contradiction `0 reviewed (covered count=2)`.
    Name the real cause and the fix instead."""
    cov = {"coverage": "covered", "reviewed_count": 2, "head_sha": "oldhead0deadbeef"}
    reason = _merge._coverage_refused_reason(cov, "newhead0cafef00d")
    assert "oldhead0" in reason
    assert "newhead0" in reason
    assert "re-run the review verb" in reason


def test_refusal_reason_never_claims_zero_when_the_count_is_positive():
    """The contradiction guard. `0 reviewed (covered count=2)` is not a message
    a reader can act on; it is a message a reader has to explain away."""
    cov = {"coverage": "covered", "reviewed_count": 2, "head_sha": "oldhead"}
    assert "0 reviewed" not in _merge._coverage_refused_reason(cov, "newhead")


def test_moved_head_keeps_the_generic_message_for_legacy_attestations_too():
    """x-e601: a pre-branch-field pass dies with the head move it cannot be
    scoped past, and the producer emits no verdict for it (an unbranchable
    line is skipped, never guessed onto this PR's row). The refusal therefore
    keeps ONE staleness message for every cohort: a targeted legacy branch
    here could fire only on a degraded recompute returning a pre-move row,
    and implied the producer names a shape it no longer emits."""
    cov = {
        "coverage": "covered",
        "reviewed_count": 0,
        "head_sha": "oldhead0deadbeef",
        "verdicts": [
            {
                "producer": "local_attestation",
                "name": "code-review",
                "verdict": "reviewed",
                "reviewed_sha": "oldhead0deadbeef",
                "scope": "legacy_head_match",
            }
        ],
    }
    reason = _merge._coverage_refused_reason(cov, "newhead0cafef00d")
    assert "unscoped attestation" not in reason
    assert "head-pinned by design" in reason
    assert "re-run the review verb" in reason


def test_moved_head_keeps_the_generic_message_for_branch_scoped_passes():
    """A branch-scoped pass under a moved head is the ordinary staleness case
    and keeps the ordinary message; no cohort gets a special-case branch."""
    cov = {
        "coverage": "covered",
        "reviewed_count": 0,
        "head_sha": "oldhead0deadbeef",
        "verdicts": [
            {
                "producer": "local_attestation",
                "name": "code-review",
                "verdict": "reviewed",
                "reviewed_sha": "oldhead0deadbeef",
                "scope": "attested_branch",
            }
        ],
    }
    reason = _merge._coverage_refused_reason(cov, "newhead0cafef00d")
    assert "unscoped attestation" not in reason
    assert "head-pinned by design" in reason


def test_genuine_zero_refusal_names_the_missing_evidence():
    """A true zero says WHAT is missing (a head-pinned pass) and what to do, not
    just that the number is zero. It must not mention attestation origin:
    volunteering it here is what put the idea in the reader's head."""
    cov = {"coverage": "covered", "reviewed_count": 0, "head_sha": "abc"}
    reason = _merge._coverage_refused_reason(cov)
    assert "head-pinned" in reason
    assert "review verb" in reason
    assert "origin" not in reason


def test_genuine_zero_names_an_absent_reviewer_and_its_consequence():
    """Parity with the Rust receipt, and this is the twin that matters more: the
    merge gate here reads coverage only, so a worker who follows a bare "run the
    review verb" while a configured reviewer is still out self-attests and merges
    ahead of a bot that never spoke. Name who is outstanding, and say what
    covering locally would do - do not steer to either."""
    cov = {
        "coverage": "covered",
        "reviewed_count": 0,
        "head_sha": "abc",
        "verdicts": [
            {"producer": "github_app", "name": "gemini-code-assist", "verdict": "absent"},
            {"producer": "github_app", "name": "chatgpt-codex-connector", "verdict": "refused"},
        ],
    }
    reason = _merge._coverage_refused_reason(cov)
    assert "waiting on gemini-code-assist" in reason
    # Never prescribes a local re-attest while someone is outstanding: this gate
    # reads coverage alone, so a worker that self-attests past a REQUIRED bot
    # lands the PR before its blocking finding posts, and this line cannot tell
    # required from optional.
    assert "review verb" not in reason
    # But it is not a bare wait either - an absent App that is never installed
    # would strand the PR - so a safe move is always named.
    assert "check config.review" in reason
    # The refused reviewer is not misreported as something we are waiting for.
    assert "chatgpt-codex-connector" not in reason


def test_uncovered_still_names_the_cause_and_the_next_move():
    """x-5b99 renamed a zero count from `covered` to `uncovered`, which sent the
    most common refusal there is down the early-return that prints only the
    word. Every test above pins the rich message against the LEGACY shape, so
    nothing caught it. The word is now a prefix, not a replacement."""
    cov = {
        "coverage": "uncovered",
        "reviewed_count": 0,
        "head_sha": "abc",
        "verdicts": [
            {"producer": "github_app", "name": "gemini-code-assist", "verdict": "absent"}
        ],
    }
    reason = _merge._coverage_refused_reason(cov)
    assert "uncovered" in reason
    assert "waiting on gemini-code-assist" in reason
    assert "check config.review" in reason


def test_a_stale_reviewer_is_named_and_asked_to_re_read():
    """A reviewer that read an older commit is neither absent nor refused, and
    "run the review verb at HEAD" is the one instruction that does not get it to
    look again. Four zeros and a wrong next step is the absence-shaped lie the
    stale verdict exists to delete."""
    cov = {
        "coverage": "uncovered",
        "reviewed_count": 0,
        "head_sha": "abc",
        "verdicts": [
            {
                "producer": "github_app",
                "name": "chatgpt-codex-connector",
                "verdict": "stale",
                "reviewed_sha": "8e557ccd",
                "freshness": "stale",
            }
        ],
    }
    reason = _merge._coverage_refused_reason(cov)
    assert "chatgpt-codex-connector reviewed an older commit" in reason
    assert "review verb" not in reason


def test_malformed_verdicts_do_not_raise():
    """Every other event-log read in this file is defensive; a truncated or
    hand-edited log must still produce a blocked receipt, not a traceback."""
    for bad in ({"a": 1}, ["nope", 3], None, "verdicts"):
        cov = {"coverage": "covered", "reviewed_count": 0, "head_sha": "abc", "verdicts": bad}
        assert "0 reviewed" in _merge._coverage_refused_reason(cov)


def test_genuine_zero_prescribes_the_verb_when_nobody_is_outstanding():
    """No absent verdict -> the local verb is the unqualified next step."""
    cov = {"coverage": "covered", "reviewed_count": 0, "head_sha": "abc", "verdicts": []}
    reason = _merge._coverage_refused_reason(cov)
    assert "run the review verb at HEAD" in reason
    assert "waiting on" not in reason


def test_refusal_reason_names_where_it_searched():
    """x-f43c: the absence branch names the logs it read.

    It used to assert "no gate evaluated this PR", a conclusion the code cannot
    support - the gate may well have evaluated it, into a different worktree's
    events log. Two workers read that line as a policy problem and set about
    designing around a gate that was already green.
    """
    reason = _merge._coverage_refused_reason(None, None, ["/a/.fno/events.jsonl"])
    assert "no review_coverage event" in reason
    assert "/a/.fno/events.jsonl" in reason
    assert "no gate evaluated" not in reason
    # sources is optional: every existing caller still type-checks.
    assert "no review_coverage event" in _merge._coverage_refused_reason(None)
    assert "unknown" in _merge._coverage_refused_reason({"coverage": "unknown"})


def test_stale_head_blocked_receipt_carries_the_cause(enabled, monkeypatch, capsys, tmp_path):
    """End to end through run_merge: the emitted blocked receipt - the line a
    worker actually reads - names the moved head, not a bare count."""
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: ({"coverage": "covered", "reviewed_count": 2, "head_sha": "oldhead0"}, ""),
    )
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: "newhead0")
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    reason = _last_json(capsys, stream="err")["reason"]
    assert "0 reviewed" not in reason
    assert "oldhead0" in reason and "newhead0" in reason


# ---- x-f43c: coverage attested in a worktree, merge run from canonical ----


def _git_repo(root, remote):
    """A checkout with a remote, so the coverage reader can resolve its slug."""
    import subprocess

    root.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "init", "-q"], cwd=str(root), check=True)
    subprocess.run(["git", "remote", "add", "origin", remote], cwd=str(root), check=True)
    (root / ".fno").mkdir(exist_ok=True)
    return root


def _global_events_at(monkeypatch, root):
    """Point the global events journal at a tmp dir and return it.

    Patches the resolver the code actually calls rather than an env var:
    ``FNO_STATE_DIR`` is not a knob anything here reads, so setting it leaves
    these tests pointed at the developer's real ``~/.fno`` - where the absence
    of any matching event makes a negative assertion pass for the wrong reason.
    """
    from fno import paths

    root.mkdir(parents=True, exist_ok=True)
    monkeypatch.setattr(paths, "global_events_json", lambda: root / "events.jsonl")
    return root


def _write_coverage(events_path, pr, *, repo=None, ts="2026-08-08T02:35:30Z", count=1):
    data = {"pr": pr, "coverage": "covered", "reviewed_count": count, "head_sha": "a3f4b413b"}
    if repo is not None:
        data["repo"] = repo
    events_path.parent.mkdir(parents=True, exist_ok=True)
    with open(events_path, "a", encoding="utf-8") as fh:
        fh.write(
            json.dumps({"ts": ts, "type": "review_coverage", "source": "hook", "data": data})
            + "\n"
        )


def test_worktree_attestation_is_visible_from_canonical(monkeypatch, tmp_path):
    """THE REPRO (PR #781). A worker attests coverage inside a worktree, then
    runs the merge gate from canonical. That refused, because the stop hook
    writes the events file of the directory it runs in while the gate reads the
    events file of the directory the merge runs in - so a satisfied gate read as
    an unsatisfiable one, and the worker escalated a green PR.
    """
    remote = "git@github.com:bllshttng/footnote.git"
    canonical = _git_repo(tmp_path / "canonical", remote)
    worktree = _git_repo(tmp_path / "wt-x-4c98", remote)
    global_dir = _global_events_at(monkeypatch, tmp_path / "home" / ".fno")

    # The review ran in the worktree; emit_to_both writes its project log and
    # the one global log.
    _write_coverage(worktree / ".fno" / "events.jsonl", 781, repo="github.com/bllshttng/footnote")
    _write_coverage(global_dir / "events.jsonl", 781, repo="github.com/bllshttng/footnote")

    # Canonical's own log never saw it. That absence is the whole specimen.
    assert not (canonical / ".fno" / "events.jsonl").exists()

    cov, _note = _merge._review_coverage_for_pr(781, str(canonical))
    assert cov is not None, "coverage attested in a worktree must be visible from canonical"
    assert cov["coverage"] == "covered"
    assert cov["reviewed_count"] == 1


def test_global_coverage_is_scoped_to_this_repo(monkeypatch, tmp_path):
    """~/.fno/events.jsonl is cross-project, so `pr` alone is not a key: another
    repo's PR 781 must not satisfy this repo's gate."""
    canonical = _git_repo(tmp_path / "canonical", "git@github.com:bllshttng/footnote.git")
    global_dir = _global_events_at(monkeypatch, tmp_path / "home" / ".fno")

    _write_coverage(global_dir / "events.jsonl", 781, repo="github.com/other/repo")
    assert _merge._review_coverage_for_pr(781, str(canonical))[0] is None


def test_global_coverage_without_repo_field_is_not_matched(monkeypatch, tmp_path):
    """Events predating the `repo` field carry no attribution, so they cannot be
    claimed by any repo scanning the shared log. Fail closed, not by guess."""
    canonical = _git_repo(tmp_path / "canonical", "git@github.com:bllshttng/footnote.git")
    global_dir = _global_events_at(monkeypatch, tmp_path / "home" / ".fno")

    _write_coverage(global_dir / "events.jsonl", 781, repo=None)
    assert _merge._review_coverage_for_pr(781, str(canonical))[0] is None


def test_project_log_still_wins_when_it_is_newer(monkeypatch, tmp_path):
    """A project-only event from an older binary must not be shadowed by a stale
    global one: newest ts wins across the two logs."""
    canonical = _git_repo(tmp_path / "canonical", "git@github.com:bllshttng/footnote.git")
    global_dir = _global_events_at(monkeypatch, tmp_path / "home" / ".fno")

    _write_coverage(
        global_dir / "events.jsonl", 781, repo="github.com/bllshttng/footnote", ts="2026-08-08T01:00:00Z", count=1
    )
    _write_coverage(
        canonical / ".fno" / "events.jsonl", 781, ts="2026-08-08T02:00:00Z", count=5
    )

    cov, _note = _merge._review_coverage_for_pr(781, str(canonical))
    assert cov is not None and cov["reviewed_count"] == 5


def test_same_named_repos_under_different_owners_do_not_share_coverage(monkeypatch, tmp_path):
    """codex P1: the global log key must be the FULL repo identity.

    `org-a/widget` and `org-b/widget` both reduce to `widget` under a last-path-
    segment slug. With a shared PR number that let one repo's coverage satisfy
    the other's auto-merge guard, and a fork shares head SHAs so the staleness
    check would not catch it either.
    """
    canonical = _git_repo(tmp_path / "canonical", "git@github.com:org-a/widget.git")
    global_dir = _global_events_at(monkeypatch, tmp_path / "home" / ".fno")

    _write_coverage(global_dir / "events.jsonl", 781, repo="github.com/org-b/widget")
    assert _merge._review_coverage_for_pr(781, str(canonical))[0] is None

    # The same repo's own identity still resolves.
    _write_coverage(global_dir / "events.jsonl", 781, repo="github.com/org-a/widget")
    assert _merge._review_coverage_for_pr(781, str(canonical))[0] is not None


def test_repo_identity_agrees_across_remote_forms():
    """Every clone form of one repo must produce one key, or the reader stops
    finding the writer's events. Mirrors the Rust parity test."""
    from fno.paths import repo_identity_from_remote_url as ident

    want = "github.com/org-a/widget"
    for url in (
        "git@github.com:org-a/widget.git",
        "https://github.com/org-a/widget",
        "https://github.com/org-a/widget.git/",
        "ssh://git@github.com:22/org-a/widget.git",
        "https://user:token@github.com/org-a/widget.git",
        "GIT@GitHub.com:Org-A/Widget.git",
    ):
        assert ident(url) == want, url

    # Too few segments, or no host: unusable as a cross-project key.
    assert ident("/local/path/widget.git") is None
    assert ident("git@github.com:widget.git") is None
    assert ident("") is None


def test_equal_timestamps_take_the_safer_verdict(monkeypatch, tmp_path):
    """codex P2: timestamps are second-precision and emit_to_both writes both
    logs in the same instant, so a strict `>` made "project log wins" a silent
    tiebreak - letting a stale covered event outrank a same-second unknown and
    clear a guard whose whole job is to fail closed."""
    canonical = _git_repo(tmp_path / "canonical", "git@github.com:org-a/widget.git")
    global_dir = _global_events_at(monkeypatch, tmp_path / "home" / ".fno")

    ts = "2026-08-08T02:35:30Z"
    _write_coverage(canonical / ".fno" / "events.jsonl", 781, ts=ts, count=1)
    events = global_dir / "events.jsonl"
    events.write_text(
        json.dumps({
            "ts": ts,
            "type": "review_coverage",
            "data": {
                "pr": 781, "coverage": "unknown", "head_sha": "a3f4b413b",
                "repo": "github.com/org-a/widget",
            },
        }) + "\n",
        encoding="utf-8",
    )

    cov, _note = _merge._review_coverage_for_pr(781, str(canonical))
    assert cov is not None and cov["coverage"] == "unknown"


def test_repo_rooted_at_the_global_log_still_scopes(monkeypatch, tmp_path):
    """When the git top-level IS the state dir (a `git init ~` dotfiles
    checkout), the project log and the global journal are ONE file. The project
    read is normally unscoped because it is repo-local; here that would let any
    repo's PR N satisfy this repo's guard, so the unscoped read must drop out."""
    from fno import paths

    root = _git_repo(tmp_path / "home", "git@github.com:org-a/widget.git")
    events = root / ".fno" / "events.jsonl"
    monkeypatch.setattr(paths, "global_events_json", lambda: events)

    _write_coverage(events, 781, repo="github.com/org-b/widget")
    assert _merge._review_coverage_for_pr(781, str(root))[0] is None, (
        "a foreign repo's event must not satisfy the guard even when the two "
        "logs are the same file"
    )

    _write_coverage(events, 781, repo="github.com/org-a/widget")
    assert _merge._review_coverage_for_pr(781, str(root))[0] is not None


def test_corrupt_line_does_not_wedge_the_gate(monkeypatch, tmp_path):
    """A malformed line in a months-old append-only log must not hide a real
    attestation: read_events raised on the first bad line, and every caller
    turns an exception into a refusal, so one corrupt byte would have wedged
    merges permanently and without explanation."""
    canonical = _git_repo(tmp_path / "canonical", "git@github.com:bllshttng/footnote.git")
    _global_events_at(monkeypatch, tmp_path / "home" / ".fno")

    events = canonical / ".fno" / "events.jsonl"
    events.write_text('{"type": "review_coverage", TRUNCATED\n', encoding="utf-8")
    _write_coverage(events, 781)

    cov, _note = _merge._review_coverage_for_pr(781, str(canonical))
    assert cov is not None and cov["coverage"] == "covered"


# ---- plan fidelity guard (x-cbab) -------------------------------------------
#
# The merge-gate half of AC5: a plan whose declared deliverables did not all ship
# refuses the merge unless each shortfall carries a carveout. Tested here in
# isolation from the stop-gate half (loopcheck.rs) - the two readers are
# independent by design, so a shared helper would let one drift while the other
# stayed green.


def _fid_plan_path(pr, *a, **k):
    return "/x/plan.md"


def test_plan_path_for_pr_is_scoped_to_this_repo(monkeypatch):
    """The ledger is global and PR numbers are per-repo, so a bare number can
    match a foreign repo's row. _plan_path_for_pr scopes by the row's pr_url so
    the merge gate never evaluates a foreign plan."""
    import fno.scoreboard.fold as fold

    rows = [
        {"pr_number": 42, "pr_url": "https://github.com/other/repo/pull/42",
         "plan_path": "/foreign/plan.md"},
        {"pr_number": 42, "pr_url": "https://github.com/bllshttng/footnote/pull/42",
         "plan_path": "/local/plan.md"},
    ]
    monkeypatch.setattr(fold, "load_ledger_rows", lambda *a, **k: rows)
    # Without the repo scope the foreign row (first) would win.
    assert _merge._plan_path_for_pr(42, repo="bllshttng/footnote") == "/local/plan.md"
    assert _merge._plan_path_for_pr(42, repo="other/repo") == "/foreign/plan.md"


def test_plan_path_for_pr_considers_a_row_with_no_pr_url(monkeypatch):
    """A row missing pr_url is not silently dropped by the repo filter."""
    import fno.scoreboard.fold as fold

    monkeypatch.setattr(
        fold, "load_ledger_rows",
        lambda *a, **k: [{"pr_number": 42, "pr_url": None, "plan_path": "/p.md"}],
    )
    assert _merge._plan_path_for_pr(42, repo="bllshttng/footnote") == "/p.md"


def test_fidelity_guard_blocks_an_uncovered_shortfall(enabled, monkeypatch, capsys, tmp_path):
    """AC5: an unjoined planned row with no covering carveout refuses the merge."""
    import fno.plan.fidelity as fid

    monkeypatch.setattr(_merge, "_review_lane_configured", lambda repo, pr_number=0: False)
    monkeypatch.setattr(_merge, "_plan_path_for_pr", _fid_plan_path)
    monkeypatch.setattr(_merge, "_pr_payload_is_code", lambda repo, pr_number=0: True)
    monkeypatch.setattr(
        fid, "compute_plan_fidelity",
        lambda *, plan_path, **k: {"refused": True, "reason": "1 unjoined, 0 carveouts"},
    )
    rc = _merge.run_merge(["42"], cwd=str(tmp_path))
    assert rc == 2
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "blocked"
    assert "plan fidelity refused" in obj["reason"]


def test_fidelity_guard_proceeds_when_the_shortfall_is_covered(enabled, monkeypatch, tmp_path):
    """The same plan with a covering carveout (refused=False) reaches the merge."""
    import fno.plan.fidelity as fid

    monkeypatch.setattr(_merge, "_review_lane_configured", lambda repo, pr_number=0: False)
    monkeypatch.setattr(_merge, "_plan_path_for_pr", _fid_plan_path)
    monkeypatch.setattr(_merge, "_pr_payload_is_code", lambda repo, pr_number=0: True)
    monkeypatch.setattr(
        fid, "compute_plan_fidelity", lambda *, plan_path, **k: {"refused": False},
    )
    fake = FakeRun(gh_merge=Result(0, "Merged PR 42", ""))
    monkeypatch.setattr(_merge, "run", fake)
    rc = _merge.run_merge(["42"], cwd=str(tmp_path))
    assert rc == 0  # reached the merge; fidelity did not block


def test_fidelity_guard_skipped_when_the_pr_carries_no_plan(enabled, monkeypatch, tmp_path):
    """No plan_path -> no denominator to check -> the guard is a no-op."""
    import fno.plan.fidelity as fid

    monkeypatch.setattr(_merge, "_review_lane_configured", lambda repo, pr_number=0: False)
    monkeypatch.setattr(_merge, "_plan_path_for_pr", lambda pr, *a, **k: None)
    monkeypatch.setattr(
        fid, "compute_plan_fidelity",
        lambda *a, **k: pytest.fail("fidelity must not run without a plan"),
    )
    fake = FakeRun(gh_merge=Result(0, "Merged PR 42", ""))
    monkeypatch.setattr(_merge, "run", fake)
    rc = _merge.run_merge(["42"], cwd=str(tmp_path))
    assert rc == 0


def test_fidelity_guard_degrades_open_on_a_probe_crash(enabled, monkeypatch, capsys, tmp_path):
    """A broken fidelity probe must not wedge a green merge: fail open, not block."""
    import fno.plan.fidelity as fid

    monkeypatch.setattr(_merge, "_review_lane_configured", lambda repo, pr_number=0: False)
    monkeypatch.setattr(_merge, "_plan_path_for_pr", _fid_plan_path)
    monkeypatch.setattr(_merge, "_pr_payload_is_code", lambda repo, pr_number=0: True)

    def _boom(*a, **k):
        raise RuntimeError("fidelity exploded")

    monkeypatch.setattr(fid, "compute_plan_fidelity", _boom)
    fake = FakeRun(gh_merge=Result(0, "Merged PR 42", ""))
    monkeypatch.setattr(_merge, "run", fake)
    rc = _merge.run_merge(["42"], cwd=str(tmp_path))
    assert rc == 0  # degraded open, merge proceeded
