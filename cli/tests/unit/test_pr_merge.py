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
from fno.pr import _merge
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
    ) -> None:
        self.gh_merge = gh_merge or Result(0, "", "")
        self.view_fails = view_fails
        self.merged_at = merged_at
        self.view_url = view_url
        self.api_ok = api_ok
        self.toplevel = toplevel
        self.behind_by = behind_by
        self.calls: list[list[str]] = []

    def __call__(self, cmd, *, cwd=None, env=None, input_text=None, timeout=None):
        cmd = list(cmd)
        self.calls.append(cmd)
        tool = cmd[0]
        if tool == "git":
            if cmd[1:3] == ["rev-parse", "--show-toplevel"]:
                return Result(0, (self.toplevel or cwd or "") + "\n", "")
            return Result(0, "", "")
        if tool == "gh":
            if cmd[1:3] == ["pr", "merge"]:
                return self.gh_merge
            if cmd[1:3] == ["pr", "view"]:
                if "mergedAt" in cmd:
                    if self.view_fails:
                        return Result(1, "", "gh: could not reach api.github.com")
                    return Result(0, self.merged_at + "\n", "")
                if "baseRefName,headRefName" in cmd:
                    return Result(
                        0,
                        json.dumps({"baseRefName": "main", "headRefName": "feature/x"}) + "\n",
                        "",
                    )
                return Result(0, self.view_url + "\n", "")
            if cmd[1] == "api":
                if len(cmd) > 2 and "/compare/" in cmd[2]:
                    return Result(0, f"{self.behind_by}\n", "")
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
        lambda pr, repo: {"coverage": "covered", "reviewed_count": 1},
    )
    monkeypatch.setattr(_merge, "_review_lane_configured", lambda repo: True)


def _last_json(capsys, *, stream="out") -> dict:
    cap = capsys.readouterr()
    text = cap.out if stream == "out" else cap.err
    return json.loads(text.strip().splitlines()[-1])


# ---- arg validation ----


def test_unknown_arg_exits_1(capsys):
    assert _merge.run_merge(["--bogus"]) == 1


def test_missing_pr_exits_1(capsys):
    assert _merge.run_merge([]) == 1


def test_legacy_invoker_flag_is_accepted_not_rejected(monkeypatch, capsys):
    # x-04ab removed --invoker; a lingering legacy flag is silently accepted
    # (never an error). The merge proceeds and is gated only by `enabled`, so
    # with auto-merge off it skips (exit 2) exactly as a no-flag call would.
    monkeypatch.setattr(_merge, "_load_auto_merge", lambda: AutoMergeBlock(enabled=False))
    assert _merge.run_merge(["--invoker=anything", "42"]) == 2
    assert _last_json(capsys)["outcome"] == "skipped"


def test_invalid_pr_number_exits_1_with_failed_json_on_stderr(capsys):
    assert _merge.run_merge(["0"]) == 1
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "failed"
    assert "invalid pr number" in obj["reason"]


# ---- config + gh gates ----


def test_auto_merge_disabled_skips_exit_2(monkeypatch, capsys):
    monkeypatch.setattr(_merge, "_load_auto_merge", lambda: AutoMergeBlock(enabled=False))
    assert _merge.run_merge(["42"]) == 2
    obj = _last_json(capsys)
    assert obj["outcome"] == "skipped"
    assert obj["pr"] == 42


def test_gh_missing_exits_127(monkeypatch, capsys, tmp_path):
    monkeypatch.setattr(_merge, "_load_auto_merge", lambda: AutoMergeBlock(enabled=True))
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


def test_merge_queued_exit_0(enabled, monkeypatch, capsys, tmp_path):
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(
        gh_merge=Result(0, "Pull request #42 will be automatically merged", ""),
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "queued"


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
    api_calls = [c for c in fake.calls if c[:2] == ["gh", "api"]]
    assert api_calls and "PUT" in api_calls[0]


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
    """gh rejects ``--auto`` (repo feature off) but accepts a plain merge."""

    def __init__(self, *, rollup=None, head_moved=False, live_head="pushed9999", **kw):
        super().__init__(**kw)
        self.merge_cmds: list[list[str]] = []
        self.rollup = rollup if rollup is not None else _rollup("SUCCESS", "SUCCESS")
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


def test_auto_merge_unsupported_repo_merges_when_checks_are_green(
    enabled, monkeypatch, capsys, tmp_path
):
    """``--auto`` is a request to QUEUE; a repo without the feature rejects it.

    The retry is only safe because the checks are read here first - ``--auto``
    is the ONLY thing enforcing require_checks_pass, so dropping it blind would
    turn "do not merge red" into "merge anything".
    """
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    fake = _AutoMergeRejectingRun(toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    obj = _last_json(capsys)
    assert obj["outcome"] == "merged"
    assert "auto-merge disabled" in obj["reason"]

    assert len(fake.merge_cmds) == 2
    assert "--auto" in fake.merge_cmds[0]
    assert "--auto" not in fake.merge_cmds[1]


def test_the_retry_pins_the_head_the_verdict_was_read_from(
    enabled, monkeypatch, capsys, tmp_path
):
    """Without ``--auto`` nothing re-checks at merge time, so the SHA is pinned.

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
    retry = fake.merge_cmds[1]
    assert "--match-head-commit" in retry
    assert retry[retry.index("--match-head-commit") + 1] == "abc123def456"


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

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 1
    assert "unreadable" in _last_json(capsys, stream="err")["reason"]
    assert len(fake.merge_cmds) == 1


def test_auto_merge_unsupported_repo_refuses_a_red_pr(
    enabled, monkeypatch, capsys, tmp_path
):
    """The load-bearing case: losing the queue must not lose the red guard."""
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
    assert len(fake.merge_cmds) == 1
    assert "--auto" in fake.merge_cmds[0]


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
    assert len(fake.merge_cmds) == 1


def test_auto_merge_unsupported_repo_fails_closed_on_an_unreadable_rollup(
    enabled, monkeypatch, capsys, tmp_path
):
    """No verdict is not a green light."""
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    fake = _AutoMergeRejectingRun(rollup={}, toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 1
    assert "unknown" in _last_json(capsys, stream="err")["reason"]
    assert len(fake.merge_cmds) == 1


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
            if cmd[:3] == ["gh", "pr", "view"] and any(
                "statusCheckRollup" in a for a in cmd
            ):
                return Result(0, "not json at all", "")
            return super().__call__(
                cmd, cwd=cwd, env=env, input_text=input_text, timeout=timeout
            )

    fake = _BadRollup(toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 1
    reason = _last_json(capsys, stream="err")["reason"]
    assert "unparseable" in reason, reason


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
            if cmd[:3] == ["gh", "pr", "view"] and any(
                "statusCheckRollup" in a for a in cmd
            ):
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


def test_coverage_missing_refuses(enabled, monkeypatch, capsys, tmp_path):
    """No review_coverage event -> Unknown -> the sanctioned merge refuses."""
    monkeypatch.setattr(_merge, "_review_coverage_for_pr", lambda pr, repo: None)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "blocked"
    assert "unreviewed" in obj["reason"]


def test_coverage_zero_refuses(enabled, monkeypatch, capsys, tmp_path):
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo: {"coverage": "covered", "reviewed_count": 0, "head_sha": "abc"},
    )
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    assert _last_json(capsys, stream="err")["outcome"] == "blocked"


def test_coverage_unknown_refuses(enabled, monkeypatch, capsys, tmp_path):
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo: {"coverage": "unknown", "head_sha": "abc"},
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
        lambda pr, repo: {"coverage": "covered", "reviewed_count": 1, "head_sha": "abc"},
    )
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"


def test_coverage_stale_head_refuses(enabled, monkeypatch, capsys, tmp_path):
    """Coverage pinned a head that no longer matches the PR head -> stale -> refuse."""
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo: {"coverage": "covered", "reviewed_count": 2, "head_sha": "oldhead"},
    )
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: "newhead")
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    assert _last_json(capsys, stream="err")["outcome"] == "blocked"


def test_covered_head_pins_the_auto_merge_cmd(monkeypatch, tmp_path):
    """x-0eaf: the covered head pins the --auto merge so a racing push cannot
    queue an unreviewed head via GitHub's auto-merge."""
    (tmp_path / ".fno").mkdir()
    _checks_enabled(monkeypatch)
    monkeypatch.setattr(_merge.shutil, "which", lambda _x: "/usr/bin/gh")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo: {"coverage": "covered", "reviewed_count": 1, "head_sha": "coveredSHA"},
    )
    monkeypatch.setattr(_merge, "_review_lane_configured", lambda repo: True)
    fake = _AutoMergeRejectingRun(
        rollup=_rollup("SUCCESS", "coveredSHA"), toplevel=str(tmp_path)
    )
    monkeypatch.setattr(_merge, "run", fake)
    _merge.run_merge(["42"], cwd=str(tmp_path))
    auto_cmd = fake.merge_cmds[0]
    assert "--auto" in auto_cmd, "expected the --auto attempt first"
    i = auto_cmd.index("--match-head-commit")
    assert auto_cmd[i + 1] == "coveredSHA"
