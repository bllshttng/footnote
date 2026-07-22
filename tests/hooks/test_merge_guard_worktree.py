#!/usr/bin/env python3
"""Tests for the gh-pr-merge guard's worktree session resolution.

Run: python3 tests/hooks/test_merge_guard_worktree.py
 or: pytest tests/hooks/test_merge_guard_worktree.py

Regression for the merge-guard-worktree-resolution fix: the guard in
hooks/git-protection.py used to resolve the active target session only from
the hook's own cwd (`git rev-parse --show-toplevel`). When `/target` runs in
a git worktree but the Claude conversation cwd is pinned to the canonical
checkout, the canonical target-state.md is a stale/unrelated session, so the
guard returned None and blocked an otherwise-authorized auto-merge. The fix
enumerates `git worktree list` and finds the active (IN_PROGRESS, fresh)
session in any worktree.
"""
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
HOOK_PATH = REPO_ROOT / "hooks" / "git-protection.py"

_spec = importlib.util.spec_from_file_location("git_protection", HOOK_PATH)
git_protection = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(git_protection)


def _git(cwd, *args):
    subprocess.run(["git", *args], cwd=cwd, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def _write_state(repo, *, status, sid, auto_merge="true", ext="true"):
    d = repo / ".fno"
    d.mkdir(parents=True, exist_ok=True)
    (d / "target-state.md").write_text(
        "---\n"
        f"status: {status}\n"
        f"session_id: {sid}\n"
        f"auto_merge_approved: {auto_merge}\n"
        f"external_review_passed: {ext}\n"
        "---\n"
    )


def _write_external_artifact(repo, sid, pr_number=356):
    d = repo / ".fno" / "artifacts"
    d.mkdir(parents=True, exist_ok=True)
    (d / f"external-{sid}.md").write_text(
        "---\n"
        "phase: external\n"
        f"session_id: {sid}\n"
        f"pr_number: {pr_number}\n"
        "---\n# external artifact\n"
    )


def _setup_canonical_plus_worktree(td):
    """Build a canonical repo (stale COMPLETE state) + a worktree with an
    active IN_PROGRESS session + external artifact. Returns (canonical, wt)."""
    canonical = Path(td) / "canonical"
    canonical.mkdir()
    _git(canonical, "init", "-q")
    _git(canonical, "config", "user.email", "t@t.t")
    _git(canonical, "config", "user.name", "t")
    (canonical / "README.md").write_text("x\n")
    _git(canonical, "add", "README.md")
    _git(canonical, "commit", "-qm", "init")
    # Canonical carries a stale, non-authorizing session.
    _write_state(canonical, status="COMPLETE", sid="old-canonical-sid")

    wt = Path(td) / "wt"
    _git(canonical, "worktree", "add", "-q", "-b", "feature/x", str(wt))
    return canonical, wt


def _call_in(cwd, fn, *args):
    prev = os.getcwd()
    os.chdir(cwd)
    try:
        return fn(*args)
    finally:
        os.chdir(prev)


def test_worktree_active_session_authorizes_merge():
    """Active IN_PROGRESS session in a worktree authorizes the merge even when
    the hook's cwd is the canonical checkout (stale COMPLETE state)."""
    with tempfile.TemporaryDirectory() as td:
        canonical, wt = _setup_canonical_plus_worktree(td)
        sid = "wt-active-sid"
        _write_state(wt, status="IN_PROGRESS", sid=sid)
        _write_external_artifact(wt, sid, pr_number=356)

        reason = _call_in(canonical, git_protection._check_pr_merge_allowed,
                          "gh pr merge 356 --merge")
        assert reason, (
            "guard should authorize via the worktree's active session; got None"
        )


def test_no_active_session_anywhere_blocks():
    """No worktree with an IN_PROGRESS session -> still blocked."""
    with tempfile.TemporaryDirectory() as td:
        canonical, wt = _setup_canonical_plus_worktree(td)
        # worktree state is also COMPLETE -> nothing authorizes.
        _write_state(wt, status="COMPLETE", sid="wt-done-sid")

        reason = _call_in(canonical, git_protection._check_pr_merge_allowed,
                          "gh pr merge 356 --merge")
        assert reason is None, f"expected block (None), got: {reason!r}"


def test_worktree_session_without_artifact_blocks():
    """Active worktree session but missing external artifact -> blocked."""
    with tempfile.TemporaryDirectory() as td:
        canonical, wt = _setup_canonical_plus_worktree(td)
        _write_state(wt, status="IN_PROGRESS", sid="wt-noart-sid")
        # no artifact written

        reason = _call_in(canonical, git_protection._check_pr_merge_allowed,
                          "gh pr merge 356 --merge")
        assert reason is None, f"expected block (None), got: {reason!r}"


def test_worktree_session_without_approval_blocks():
    """Active worktree session but auto_merge_approved false -> blocked."""
    with tempfile.TemporaryDirectory() as td:
        canonical, wt = _setup_canonical_plus_worktree(td)
        sid = "wt-noapprove-sid"
        _write_state(wt, status="IN_PROGRESS", sid=sid, auto_merge="false")
        _write_external_artifact(wt, sid)

        reason = _call_in(canonical, git_protection._check_pr_merge_allowed,
                          "gh pr merge 356 --merge")
        assert reason is None, f"expected block (None), got: {reason!r}"


def test_cwd_session_still_authorizes():
    """Backward-compat: an active session in the hook's own repo_root (no
    worktree indirection) still authorizes, exactly as before."""
    with tempfile.TemporaryDirectory() as td:
        canonical = Path(td) / "canonical"
        canonical.mkdir()
        _git(canonical, "init", "-q")
        _git(canonical, "config", "user.email", "t@t.t")
        _git(canonical, "config", "user.name", "t")
        (canonical / "README.md").write_text("x\n")
        _git(canonical, "add", "README.md")
        _git(canonical, "commit", "-qm", "init")
        sid = "cwd-active-sid"
        _write_state(canonical, status="IN_PROGRESS", sid=sid)
        _write_external_artifact(canonical, sid)

        reason = _call_in(canonical, git_protection._check_pr_merge_allowed,
                          "gh pr merge 356 --merge")
        assert reason, "active session in cwd repo_root should authorize"


def test_prefer_pr_mismatch_blocks():
    """Typo / wrong-PR protection: a single active session whose artifact
    records a DIFFERENT PR than the merge command must NOT authorize it."""
    with tempfile.TemporaryDirectory() as td:
        canonical, wt = _setup_canonical_plus_worktree(td)
        sid = "wt-mismatch-sid"
        _write_state(wt, status="IN_PROGRESS", sid=sid)
        _write_external_artifact(wt, sid, pr_number=111)  # session is for 111

        reason = _call_in(canonical, git_protection._check_pr_merge_allowed,
                          "gh pr merge 999 --merge")  # but merging 999
        assert reason is None, f"wrong-PR merge should be blocked, got: {reason!r}"


def test_multi_worktree_prefer_pr_selects_matching():
    """Two active sessions in different worktrees: the merge command's PR
    number selects the session whose artifact records it."""
    with tempfile.TemporaryDirectory() as td:
        canonical, wtA = _setup_canonical_plus_worktree(td)
        wtB = Path(td) / "wtB"
        _git(canonical, "worktree", "add", "-q", "-b", "feature/y", str(wtB))
        _write_state(wtA, status="IN_PROGRESS", sid="sid-a")
        _write_external_artifact(wtA, "sid-a", pr_number=356)
        _write_state(wtB, status="IN_PROGRESS", sid="sid-b")
        _write_external_artifact(wtB, "sid-b", pr_number=999)

        sf, fm, root = _call_in(canonical, git_protection._get_active_target_session, "356")
        assert sf is not None and root == wtA.resolve(), \
            f"expected wtA selected for PR 356, got root={root}"
        assert fm and fm.get("session_id") == "sid-a", f"expected sid-a, got {fm}"

        # And the full check authorizes.
        reason = _call_in(canonical, git_protection._check_pr_merge_allowed,
                          "gh pr merge 356 --merge")
        assert reason, "PR 356 should authorize via the matching worktree session"


def test_multi_worktree_no_pr_blocks():
    """Two active sessions and no PR parseable from the command (no-arg /
    current-branch form) -> cannot disambiguate -> fail closed."""
    with tempfile.TemporaryDirectory() as td:
        canonical, wtA = _setup_canonical_plus_worktree(td)
        wtB = Path(td) / "wtB"
        _git(canonical, "worktree", "add", "-q", "-b", "feature/y", str(wtB))
        _write_state(wtA, status="IN_PROGRESS", sid="sid-a")
        _write_external_artifact(wtA, "sid-a", pr_number=356)
        _write_state(wtB, status="IN_PROGRESS", sid="sid-b")
        _write_external_artifact(wtB, "sid-b", pr_number=999)

        reason = _call_in(canonical, git_protection._check_pr_merge_allowed,
                          "gh pr merge --merge")  # no PR number
        assert reason is None, f"ambiguous multi-session merge must block, got: {reason!r}"


def test_multi_worktree_all_conflicting_blocks():
    """Two active sessions, neither artifact records the requested PR ->
    no neutral fallback exists -> fail closed."""
    with tempfile.TemporaryDirectory() as td:
        canonical, wtA = _setup_canonical_plus_worktree(td)
        wtB = Path(td) / "wtB"
        _git(canonical, "worktree", "add", "-q", "-b", "feature/y", str(wtB))
        _write_state(wtA, status="IN_PROGRESS", sid="sid-a")
        _write_external_artifact(wtA, "sid-a", pr_number=111)
        _write_state(wtB, status="IN_PROGRESS", sid="sid-b")
        _write_external_artifact(wtB, "sid-b", pr_number=222)

        reason = _call_in(canonical, git_protection._check_pr_merge_allowed,
                          "gh pr merge 356 --merge")
        assert reason is None, f"no session owns PR 356 -> block, got: {reason!r}"


def test_parse_merge_pr_forms():
    """_parse_merge_pr handles bare number, leading flags, and URL forms;
    returns None for branch-name and no-argument forms."""
    p = git_protection._parse_merge_pr
    assert p("gh pr merge 356 --merge") == "356"
    assert p("gh pr merge --squash 356") == "356"
    assert p("gh pr merge --auto --delete-branch 356") == "356"
    assert p("gh pr merge https://github.com/o/r/pull/356") == "356"
    assert p("gh pr merge https://github.com/o/r/pull/356/") == "356"
    assert p("gh pr merge") is None
    assert p("gh pr merge my-feature-branch") is None
    assert p("cd /x && gh pr merge 42 --merge") == "42"


def test_unreadable_state_path_does_not_raise():
    """A candidate whose state file can't be stat'd is skipped, not fatal."""
    # Nonexistent path exercises the guarded exists() branch.
    assert git_protection._parse_active_state(
        Path("/nonexistent/fno-test/target-state.md")) is None


# ---------------------------------------------------------------------------
# Command-position tokenization (the matcher fix)
# ---------------------------------------------------------------------------

_MERGE = "gh pr merge"  # kept out of a raw string so this file's own text
#                         never trips a loose gh-pr-merge matcher


def test_command_segments_quoted_separator_stays_intact():
    """A separator inside a quoted argument is NOT a segment boundary."""
    segs = git_protection._command_segments(
        'fno backlog update x --details "a; b && c"')
    assert len(segs) == 1
    assert segs[0][:5] == ["fno", "backlog", "update", "x", "--details"]


def test_find_merge_segment_ignores_quoted_phrase():
    """The 2026-07-06 live false positive: merge phrase inside a --details
    string, with a separator inside the quotes, must not be recognized."""
    segs = git_protection._command_segments(
        f'fno backlog update x --details "next step; {_MERGE} after review"')
    assert git_protection._find_merge_segment(segs) is None


def test_find_merge_segment_matches_command_position():
    segs = git_protection._command_segments(f"echo hi && {_MERGE} 5")
    assert git_protection._find_merge_segment(segs) == f"{_MERGE} 5"


def test_find_git_segments_catches_compound_push():
    """Closes the startswith('git') bypass: git at command position after &&."""
    segs = git_protection._command_segments("cd /tmp && git push origin main")
    assert git_protection._find_git_segments(segs) == ["git push origin main"]


def test_find_merge_segment_newline_multiline():
    """Regression: a merge on line 2 of a multi-line command must be caught
    (shlex eats newlines in whitespace_split mode, so the physical-line split
    is what makes line 2 its own segment)."""
    segs = git_protection._command_segments(f"git status\n{_MERGE} 356 --squash")
    assert git_protection._find_merge_segment(segs) is not None


def test_find_merge_segment_prefix_forms_are_caught():
    """Regression: wrapper/assignment/path/subshell prefixes must not hide the
    merge verb (the old regex-anywhere matcher caught all of these)."""
    fm = git_protection._find_merge_segment
    seg = git_protection._command_segments
    for cmd in (
        f"GH_TOKEN=x {_MERGE} 356 --squash",
        f"env {_MERGE} 356",
        f"sudo {_MERGE} 356",
        f"/usr/bin/gh pr merge 356",
        f"(gh pr merge 356)",
    ):
        assert fm(seg(cmd)) is not None, f"prefix bypass not caught: {cmd!r}"


def test_find_git_segments_prefix_forms_are_caught():
    seg = git_protection._command_segments
    fg = git_protection._find_git_segments
    for cmd in (
        "GIT_DIR=/x git push origin main",
        "sudo git push origin main",
        "/usr/bin/git push origin main",
    ):
        assert fg(seg(cmd)), f"git prefix bypass not caught: {cmd!r}"


def test_find_git_segments_pipe_does_not_false_split_git():
    """A pipe is a separator; `git log | grep x` still recognizes the git verb
    and stays allowed (grep segment is not git)."""
    segs = git_protection._command_segments("git log | grep foo")
    assert git_protection._find_git_segments(segs) == ["git log"]


def test_backslash_line_continuation_does_not_bypass():
    """Regression (gemini, PR #227): a backslash line-continuation joins two
    physical lines into one command; the gate must see it joined, not split
    with the branch target / flag judged in isolation."""
    seg = git_protection._command_segments
    fg = git_protection._find_git_segments
    fm = git_protection._find_merge_segment
    # --no-verify on the continuation line is still caught
    assert fg(seg("git commit \\\n  --no-verify -m x")) == ["git commit --no-verify -m x"]
    # branch target on the continuation line is still caught
    assert fg(seg("git push \\\n  origin main")) == ["git push origin main"]
    # merge verb split by a continuation is still caught
    assert fm(seg(f"{_MERGE} \\\n  5")) == f"{_MERGE} 5"
    # a mid-token continuation rejoins (shell semantics: removed, not spaced)
    assert fg(seg("git pu\\\nsh origin main")) == ["git push origin main"]


def test_command_segments_unbalanced_quote_raises():
    try:
        git_protection._command_segments(f'{_MERGE} 5 --body "unclosed')
    except ValueError:
        return
    raise AssertionError("expected ValueError on unbalanced quote")


# ---------------------------------------------------------------------------
# State placement + opt-out marker + flag race-safety (subprocess)
# ---------------------------------------------------------------------------

def _run_hook_subprocess(command, fno_home, cwd=None):
    env = dict(os.environ, FNO_HOME=str(fno_home))
    payload = json.dumps({"tool_name": "Bash", "tool_input": {"command": command}})
    p = subprocess.run([sys.executable, str(HOOK_PATH)], input=payload,
                       capture_output=True, text=True, env=env, cwd=cwd)
    return p.stdout, p.returncode


def test_state_writes_land_under_fno_home():
    """A blocked protected push writes git-protection.json under FNO_HOME and
    creates nothing under a harness state dir in the sandbox (AC2-HP)."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        out, _ = _run_hook_subprocess("git push origin main", fno)
        assert '"permissionDecision": "deny"' in out
        assert (fno / "git-protection.json").exists()
        assert not (Path(td) / ".claude").exists()


def test_disable_marker_short_circuits():
    """$FNO_HOME/git-protection.disabled -> exit 0, no decision (AC1-EDGE)."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        (fno / "git-protection.disabled").write_text("")
        out, rc = _run_hook_subprocess("git push origin main", fno)
        assert out.strip() == ""
        assert rc == 0


def test_no_verify_flag_consumed_once_and_missing_is_safe():
    """Approved --no-verify allows and consumes the flag; a subsequent call
    with the flag gone denies without crashing on the missing flag (AC1-FR:
    the unlink(missing_ok=True) race-safety)."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        (fno / "approve_no_verify.flag").write_text("")
        out1, _ = _run_hook_subprocess("git commit --no-verify -m x", fno)
        assert '"permissionDecision": "allow"' in out1
        assert not (fno / "approve_no_verify.flag").exists()
        out2, rc2 = _run_hook_subprocess("git commit --no-verify -m x", fno)
        assert '"permissionDecision": "deny"' in out2
        assert "Traceback" not in out2


def _run_standalone():
    failed = 0
    for name, fn in list(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"PASS  {name}")
            except AssertionError as exc:
                failed += 1
                print(f"FAIL  {name}\n      {exc}")
            except Exception as exc:
                failed += 1
                print(f"ERROR {name}\n      {type(exc).__name__}: {exc}")
    return failed


if __name__ == "__main__":
    sys.exit(_run_standalone())
