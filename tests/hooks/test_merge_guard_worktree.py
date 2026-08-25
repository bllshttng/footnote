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
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
HOOK_PATH = REPO_ROOT / "hooks" / "git-protection.py"

_spec = importlib.util.spec_from_file_location("git_protection", HOOK_PATH)
git_protection = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(git_protection)


def _git(cwd, *args):
    subprocess.run(["git", *args], cwd=cwd, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def _write_state(repo, *, status, sid, auto_merge="true", ext="true",
                 switch="true"):
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
    # x-3855: raw `gh pr merge` also needs the live switch armed. The state
    # file alone is a snapshot; without an armed config the two-factor path
    # declines (a disarm must stop a run started under the old setting).
    (d / "config.toml").write_text(f"[auto_merge]\nenabled = {switch}\n")


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


def test_live_switch_off_declines_raw_merge():
    """x-2270 at the raw path (x-3855): an active session whose manifest says
    approved and a fresh artifact still declines when the live config switch
    is off - the snapshot must not outlive the operator's disarm, or raw gh
    merges past what the sanctioned verb refuses."""
    with tempfile.TemporaryDirectory() as td:
        canonical, wt = _setup_canonical_plus_worktree(td)
        sid = "disarmed-sid"
        _write_state(wt, status="IN_PROGRESS", sid=sid, switch="false")
        _write_external_artifact(wt, sid)

        reason = _call_in(canonical, git_protection._check_pr_merge_allowed,
                          "gh pr merge 356 --merge")
        assert reason is None, "disarmed config must decline the raw merge"


def test_env_grant_stamp_arms_without_live_config():
    """x-01b9 at the raw path: a run granted at spawn
    (auto_merge_source: env-target-auto-merge) authorizes on its own, without
    the standing config switch."""
    with tempfile.TemporaryDirectory() as td:
        canonical, wt = _setup_canonical_plus_worktree(td)
        sid = "env-grant-sid"
        d = wt / ".fno"
        d.mkdir(parents=True, exist_ok=True)
        (d / "target-state.md").write_text(
            "---\n"
            "status: IN_PROGRESS\n"
            f"session_id: {sid}\n"
            "auto_merge_approved: true\n"
            "auto_merge_source: env-target-auto-merge\n"
            "external_review_passed: true\n"
            "---\n"
        )
        (d / "config.toml").write_text("[auto_merge]\nenabled = false\n")
        _write_external_artifact(wt, sid)

        reason = _call_in(canonical, git_protection._check_pr_merge_allowed,
                          "gh pr merge 356 --merge")
        assert reason, "the spawn-time env grant must authorize on its own"


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

def _run_hook_subprocess(command, fno_home, cwd=None, extra_env=None):
    env = dict(os.environ, FNO_HOME=str(fno_home))
    # These tests isolate merge-marker and worktree resolution behavior. Give
    # the new fail-closed hold veto an explicit unheld answer while preserving
    # the existing fail-open behavior of unrelated missing probe verbs.
    bin_dir = Path(fno_home).parent / "hook-bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    fno = bin_dir / "fno"
    fno.write_text(
        '#!/usr/bin/env bash\n'
        '[[ "$1 $2" == "pr hold-check" ]] && exit 0\n'
        'exit 1\n'
    )
    fno.chmod(0o755)
    env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"
    env.update(extra_env or {})
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


def _with_marker(td, name="merge-gate.disabled", age_seconds=0):
    """Create $FNO_HOME and a marker file, optionally backdated."""
    fno = Path(td) / ".fno"
    fno.mkdir(parents=True, exist_ok=True)
    marker = fno / name
    marker.write_text("")
    if age_seconds:
        past = time.time() - age_seconds
        os.utime(marker, (past, past))
    return fno, marker


# --- The merge-gate marker must NOT reach the branch / --no-verify gates ----
# These encode the operator policy stated 2026-08-07: auto-merge after the
# gates pass is fine, disabling main-push protection never is. The marker used
# to sit ahead of every gate as an unconditional exit(0), so one touch opened
# a direct push to main on every harness lane on the machine.

def test_merge_marker_does_not_open_push_to_main():
    """Marker present -> `git push origin main` is STILL DENIED."""
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td)
        out, _ = _run_hook_subprocess("git push origin main", fno)
        assert '"permissionDecision": "deny"' in out
        assert marker.exists(), "branch gate must not consume the merge marker"


def test_merge_marker_does_not_open_no_verify_push_to_main():
    """The second door to the same protection: --no-verify skips
    .git/hooks/pre-push, which IS the protected-branch guard. Excluding the
    marker from the branch check alone would leave this path open."""
    with tempfile.TemporaryDirectory() as td:
        fno, _ = _with_marker(td)
        out, _ = _run_hook_subprocess("git push --no-verify origin main", fno)
        assert '"permissionDecision": "deny"' in out


def test_merge_marker_does_not_open_no_verify_commit():
    with tempfile.TemporaryDirectory() as td:
        fno, _ = _with_marker(td)
        out, _ = _run_hook_subprocess("git commit --no-verify -m x", fno)
        assert '"permissionDecision": "deny"' in out


def test_merge_marker_does_not_open_push_to_main_in_a_compound_command():
    """A merge decision covers the merge segment only. `gh pr merge && git push
    origin main` must not ride the merge allow past the branch gate."""
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td)
        out, _ = _run_hook_subprocess(
            "gh pr merge 123 --squash && git push origin main", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert marker.exists(), "a denied command must not consume the marker"


def test_no_verify_approval_does_not_open_push_to_main_compound():
    """A deny anywhere outranks an allow anywhere, in EITHER segment order. The
    loop used to short-circuit on the first allow, so the approve flag carried
    a push to main whenever the --no-verify segment came first."""
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        for cmd in ("git commit --no-verify -m x && git push origin main",
                    "git push origin main && git commit --no-verify -m x"):
            flag.write_text("")
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd
            assert flag.exists(), f"denied command must not consume: {cmd}"


def test_merge_mixed_with_no_verify_approval_is_refused():
    """One approval authorizes one action. Mixing an approved --no-verify
    segment with a merge would need two single-use claims committed atomically
    in one tool call; refusing keeps each branch claim-free of the other, and
    keeps a merge denial from burning the operator's --no-verify approval."""
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td)
        flag = fno / "approve_no_verify.flag"
        flag.write_text("")
        out, _ = _run_hook_subprocess(
            "gh pr merge 1 --squash && git commit --no-verify -m x", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert flag.exists(), "a refused command must not consume the approval"
        assert marker.exists(), "a refused command must not consume the marker"


_BT = chr(96)  # backtick, kept out of the f-strings below for readability


def test_shell_keywords_do_not_hide_a_push_to_main():
    """A keyword occupying command position leaves the real command at the next
    token. With only {then, do} recognized, every other construct hid the push
    from both gates and the hook emitted no decision at all - each of these is
    one token away from a bare `git push origin main`."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        for cmd in ("! git push origin main",
                    "if git push origin main; then true; fi",
                    "{ git push origin main; }",
                    "while git push origin main; do true; done",
                    "until git push origin main; do true; done",
                    "for x in a; do git push origin main; done",
                    "if gh pr merge 42 --merge; then true; fi",
                    "true |& git push origin main"):
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd


def test_hooks_path_override_cannot_open_the_branch_gate():
    """The hooks door must not open the branch door, in either direction. With
    the approval flag present, a hooksPath override on a push to main returned
    allow - the exact inversion the branch-gate-first ordering exists to stop."""
    with tempfile.TemporaryDirectory() as td:
        fno, _ = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            "git -c core.hooksPath=/dev/null push origin main", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out


def test_hooks_path_check_is_positional():
    """A substring scan over quote-stripped text refused a commit whose MESSAGE
    named core.hooksPath - the same false refusal the allowlist fix removed."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        out, rc = _run_hook_subprocess(
            'git commit -m "docs: explain core.hooksPath guard"', fno, cwd=td)
        assert '"permissionDecision": "deny"' not in out and rc == 0


def test_shell_runner_cannot_smuggle_a_sibling_past_an_authorization():
    """_effective_argv re-tokenizes a runner's quoted argument so the inner verb
    is gated, but the OUTER command is still one segment - so the wrapped form
    counted as standing alone while the unwrapped form was refused."""
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td)
        out, _ = _run_hook_subprocess(
            'bash -c "gh pr merge 42 && rm -rf /tmp/zzz"', fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert marker.exists()
        for cmd in ('zsh -f -c "git push origin main"',
                    'bash -l -c "git push origin main"'):
            out2, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out2, cmd


def test_fallback_does_not_authorize_a_no_verify_allow():
    """Nothing can be counted on the unparseable fallback, so nothing may be
    authorized there either - the guard had been reasoned about for the merge
    path only, leaving the approval path to cover an arbitrary sibling."""
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            "git commit --no-verify -m 'it's ready' && rm -rf /tmp/zzz",
            fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert flag.exists()


def test_wrapper_options_do_not_hide_the_verb():
    """A wrapper that takes its own option pushed the verb past argv[0], and
    matching only the exact token `-c` covered the one `bash -c` spelling nobody
    types. `timeout` is installed on this machine, so `timeout 10 git push
    origin main` was a live one-word bypass."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        for cmd in ("bash -lc 'git push origin main'",
                    "sh -cx 'git push origin main'",
                    "bash -lc 'gh pr merge 42'",
                    "env -i git push origin main",
                    "nice -n 5 git push origin main",
                    "sudo -u x git push origin main",
                    "timeout 10 git push origin main"):
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd
        # ...without turning an ordinary wrapped push into a refusal.
        out2, rc2 = _run_hook_subprocess("timeout 10 git push origin feature/x",
                                         fno, cwd=td)
        assert '"permissionDecision": "deny"' not in out2 and rc2 == 0


def test_hooks_path_override_is_a_no_verify_by_another_name():
    """core.hooksPath disables .git/hooks/pre-push, which IS the branch guard.
    The `git config` spelling is PERSISTENT - every later commit and push is
    unguarded with no flag on them - and `config` is on the allowlist, so the
    check has to run ahead of it."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        for cmd in ("git config core.hooksPath /dev/null",
                    "git -c core.hooksPath=/dev/null push origin main",
                    "git -c core.hooksPath=/dev/null commit -m x"):
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd
        out2, rc2 = _run_hook_subprocess("git config user.name", fno, cwd=td)
        assert '"permissionDecision": "deny"' not in out2 and rc2 == 0


def test_unparseable_fallback_is_not_allowlisted_by_its_first_command():
    """On the unbalanced-quote fallback the "segment" is the WHOLE string, so a
    positional allowlist read the FIRST command's subcommand and waved the rest
    through. The fallback is meant to be deny-leaning."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        for cmd in ("git status && git push origin main 'unbal",
                    'git log --oneline && git push origin main --message "unbal'):
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd


def test_fd_duplication_is_not_a_file_write():
    """`2>&1` duplicates a descriptor and writes no file, so refusing it denied
    an ordinary gated command for nothing."""
    with tempfile.TemporaryDirectory() as td:
        fno, _ = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess("git commit --no-verify -m ok 2>&1",
                                      fno, cwd=td)
        assert '"permissionDecision": "allow"' in out


def test_heredoc_body_mentioning_a_heredoc_is_still_allowed():
    """The opener scan walked BODY lines too, so a message that merely mentioned
    <<EOF broke the escape the refusal message recommends."""
    with tempfile.TemporaryDirectory() as td:
        fno, _ = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            "git commit --no-verify -F - <<'EOF'\nfix <<EOF parsing\nEOF",
            fno, cwd=td)
        assert '"permissionDecision": "allow"' in out


def test_refspec_forms_that_reach_main_are_denied():
    """Only the `feature:main` form was normalized, so a force-prefixed or
    fully-qualified destination never compared equal to a protected branch.
    `git push origin +main` is a FORCE push to main from any branch."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        for cmd in ("git push origin +main",
                    "git push origin refs/heads/main",
                    "git push origin +refs/heads/main",
                    "git push --all origin",
                    "git push --mirror origin"):
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd


def test_git_global_options_do_not_hide_the_subcommand():
    """Both gates key on the subcommand - one by `git push` adjacency, the other
    by token position - so a global option before it hid the push from both.
    `-c core.hooksPath=...` additionally disables .git/hooks/pre-push, which IS
    the branch guard, so it is a --no-verify by another name."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        for cmd in ("git -C /repo push origin main",
                    "git --no-pager push origin main",
                    "git -c core.hooksPath=/dev/null push origin main",
                    "git -c core.hooksPath=/dev/null commit -m x"):
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd
        # A global option must not turn an ordinary push into a refusal.
        out2, rc2 = _run_hook_subprocess("git -C /repo push origin feature/x",
                                         fno, cwd=td)
        assert '"permissionDecision": "deny"' not in out2 and rc2 == 0


def test_quoted_shell_runner_argument_is_re_tokenized():
    """`eval git push ...` was caught while `eval "git push ..."` - the form
    anyone actually writes - was invisible, because the whole command sat in one
    quoted token. Same for `bash -c "..."`."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        for cmd in ('eval "git push origin main"',
                    'bash -c "git push origin main"',
                    'sh -c "git push origin main"'):
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd


def test_heredoc_check_ignores_quoted_argument_text():
    """`<<` inside an ARGUMENT is not a heredoc opener. A raw regex scan refused
    a commit whose message read "shift << 2"."""
    with tempfile.TemporaryDirectory() as td:
        fno, _ = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            'git commit --no-verify -m "shift << 2"', fno, cwd=td)
        assert '"permissionDecision": "allow"' in out


def test_unparseable_merge_reaches_the_two_factor_gate():
    """An apostrophe raises in shlex, and the lone-command rule cannot count
    segments on that fallback. Enforcing it anyway refused every fallback merge
    with a compound-command message, blocking legitimate auto-merge on routine
    prose. It must be refused by the MERGE gate, naming the real reason."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        out, _ = _run_hook_subprocess(
            "gh pr merge 12 --body \"it's ready\"", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert "two-factor check failed" in out
        assert "one approval cannot authorize" not in out


def test_case_arm_does_not_hide_the_verb():
    """`)` terminates a case arm pattern. Without it as a separator the arm body
    stayed in the case word's segment and both gates saw nothing."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        for cmd in ("case x in *) git push origin main;; esac",
                    "case x in *) gh pr merge 42 --admin;; esac",
                    "eval git push origin main",
                    "coproc git push origin main"):
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd


def test_quoted_argument_text_is_not_read_as_the_command():
    """Segments arrive shlex-rejoined with quotes stripped, so a regex allowlist
    read argument text as the command. In one direction that waved a --no-verify
    commit through because its message said "git log"; in the other, dropping the
    allowlist denied a read-only command as a push to main. The check is
    positional (token[1]), so the message text cannot decide either way."""
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        # A message naming a protected push must not be refused as one.
        for cmd in ('git commit -m "fix: block git push origin main"',
                    'git log --grep "git push origin main"'):
            out, rc = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' not in out and rc == 0, cmd
        # ...and an allowlisted word in a message must not smuggle --no-verify.
        out2, _ = _run_hook_subprocess(
            'git commit --no-verify -m "see git log"', fno, cwd=td)
        assert '"permissionDecision": "allow"' in out2, "gated, not waved through"
        assert not flag.exists(), "the approval was actually consumed"


def test_redirection_disqualifies_an_authorization():
    """An authorization covers the whole Bash call, so a `>` rides it into an
    arbitrary file overwrite no gate inspects."""
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            "git commit --no-verify -m ok > /tmp/gp-test-log", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert flag.exists()


def test_quoted_heredoc_body_may_contain_a_backtick():
    """The refusal message recommends `-F - <<'EOF'`, so that has to survive a
    markdown code span in the message. A raw substring scan for substitutions
    broke it; the scan runs over parsed tokens, which exclude heredoc bodies.
    An UNQUOTED delimiter does expand, so it stays refused."""
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        ok_cmd = (f"git commit --no-verify -F - <<'EOF'\n"
                  f"fix {_BT}foo{_BT} handling\nEOF")
        out, _ = _run_hook_subprocess(ok_cmd, fno, cwd=td)
        assert '"permissionDecision": "allow"' in out

        flag.write_text("")
        bad_cmd = (f"git commit --no-verify -F - <<EOF\n"
                   f"fix {_BT}id{_BT}\nEOF")
        out2, _ = _run_hook_subprocess(bad_cmd, fno, cwd=td)
        assert '"permissionDecision": "deny"' in out2


def test_substitution_disqualifies_an_authorization():
    """A `$(...)` body is re-segmented and trips the count, but backticks are
    skipped by _substitution_bodies and `<(` is not a separator, so both stayed
    inside ONE segment and passed the lone-command rule while running arbitrary
    code under the authorization."""
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td)
        for cmd in (f'gh pr merge 12 --squash --body "{_BT}id{_BT}"',
                    "gh pr merge 12 --squash --body-file <(id)"):
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd
            assert marker.exists(), cmd


def test_heredoc_stdin_commit_is_still_allowed():
    """The refusal message points at `-F -` with a heredoc as the way to pass a
    long message without $(cat ...), so that form must actually work."""
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            "git commit --no-verify -F - <<'EOF'\nmsg\nEOF", fno, cwd=td)
        assert '"permissionDecision": "allow"' in out
        assert not flag.exists(), "the approval is consumed on the allow"


def test_uppercase_wrapper_does_not_hide_the_verb():
    """`ENV`/`SUDO` resolve on a case-insensitive filesystem. _effective_argv's
    wrapper test was case-sensitive, so the real verb stayed at argv[1] and no
    gate ever saw the push."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        for cmd in ("ENV git push origin main", "SUDO git push origin main"):
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd


def test_unwritable_state_does_not_crash_the_deny_path():
    """save_state runs first on every protected push. An unguarded OSError would
    exit non-zero, which a PreToolUse hook treats as non-blocking - so the push
    to main would proceed. A crash here fails OPEN."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        (fno / "git-protection.json").mkdir()   # a directory where a file goes
        out, _ = _run_hook_subprocess("git push origin main", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert "Traceback" not in out


def test_branch_bypass_does_not_also_open_no_verify():
    """"One approval must not open the other door" has to hold in BOTH
    directions. Checking the branch gate first and returning safe on an approved
    push let one bypass phrase also skip .git/hooks/pre-push."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        env = {"CLAUDE_RECENT_USER_MESSAGE": "Push to Main"}
        out, _ = _run_hook_subprocess("git push --no-verify origin main",
                                      fno, cwd=td, extra_env=env)
        assert '"permissionDecision": "deny"' in out
        # The branch bypass itself still works for a plain push.
        out2, rc2 = _run_hook_subprocess("git push origin main", fno, cwd=td,
                                         extra_env=env)
        assert '"permissionDecision": "deny"' not in out2 and rc2 == 0


def test_uppercase_git_is_still_gated():
    """`GIT` resolves on a case-insensitive filesystem (macOS). The push
    patterns matched case-insensitively but extract_branch_from_push did not, so
    the branch never parsed and the push fell through to allowed."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        for cmd in ("GIT push origin main", "Git push origin main"):
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd


def test_no_verify_approval_requires_a_lone_command():
    """The lone-command rule applies to every authorizing path, not just the
    merge marker: the sibling here is not a git segment, so no gate inspects it,
    yet the approval's allow covered the whole Bash call."""
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            "git commit --no-verify -m x && gh api -X PATCH "
            "repos/o/r/git/refs/heads/main", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert flag.exists()


def test_two_factor_merge_also_requires_a_lone_command():
    """Not just the marker path. A leading `cd` is deliberately not carved out:
    an allow covers a prefix exactly as it covers a suffix."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        out, _ = _run_hook_subprocess(
            "gh pr merge 1 --squash && gh api -X PATCH "
            "repos/o/r/git/refs/heads/main", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out


def test_no_verify_approval_does_not_open_push_to_main_single_segment():
    """The protected-branch gate outranks the --no-verify approval. This is ONE
    segment, so no cross-segment rule can catch it: the evaluator checked
    --no-verify first and returned allow without ever reaching the branch
    check, so an operator-touchable flag opened main directly."""
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess("git push --no-verify origin main",
                                      fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert flag.exists(), "a denied push must not consume the approval"


def test_one_flag_cannot_authorize_several_no_verify_segments():
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            "git commit --no-verify -m a && git commit --no-verify -m b",
            fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert flag.exists()


def test_marker_override_requires_a_lone_merge():
    """A PreToolUse allow blankets the WHOLE Bash call, so a marker-authorized
    merge would approve whatever rides along - including a direct force-move of
    main via the API, which no git gate inspects."""
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td)
        out, _ = _run_hook_subprocess(
            "gh pr merge 1 --squash && gh api -X PATCH "
            "repos/o/r/git/refs/heads/main", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert marker.exists(), "a refused override must not be spent"


def test_one_marker_cannot_authorize_several_merges():
    """`gh pr merge 1 && gh pr merge 2` rode a single marker consume, and only
    the first reached the audit log."""
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td)
        out, _ = _run_hook_subprocess(
            "gh pr merge 1 --squash && gh pr merge 2 --squash", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert marker.exists()


def test_unrecordable_override_fails_closed():
    """The trail is what justifies having an override, so an unwritable log
    refuses rather than allowing unrecorded - the one case an agent could
    arrange by putting a directory at the log path."""
    with tempfile.TemporaryDirectory() as td:
        fno, _ = _with_marker(td)
        (fno / "merge-gate-overrides.log").mkdir()
        out, _ = _run_hook_subprocess("gh pr merge 9 --squash", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out


def test_legacy_disable_marker_is_inert():
    """A stale git-protection.disabled from before the rename must no longer
    bypass anything. The rename is the migration: fail-safe, no cleanup."""
    with tempfile.TemporaryDirectory() as td:
        fno, _ = _with_marker(td, name="git-protection.disabled")
        out, _ = _run_hook_subprocess("git push origin main", fno)
        assert '"permissionDecision": "deny"' in out


# --- ...but it IS the merge gate's scoped override -------------------------

def test_merge_marker_allows_merge_and_is_consumed():
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td)
        out1, _ = _run_hook_subprocess("gh pr merge 123 --squash", fno, cwd=td)
        assert '"permissionDecision": "allow"' in out1
        assert not marker.exists(), "marker must be single-use"
        log = fno / "merge-gate-overrides.log"
        assert log.exists() and "123" in log.read_text()
        out2, _ = _run_hook_subprocess("gh pr merge 123 --squash", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out2


def test_override_log_entry_cannot_be_forged_with_a_newline():
    """One consumed override = exactly one log line. A trail that an embedded
    newline can forge extra entries into is not a trail."""
    with tempfile.TemporaryDirectory() as td:
        fno, _ = _with_marker(td)
        out, _ = _run_hook_subprocess(
            'gh pr merge 123 --body "x\n2099-01-01 forged entry"', fno, cwd=td)
        assert '"permissionDecision": "allow"' in out
        lines = [ln for ln in (fno / "merge-gate-overrides.log")
                 .read_text().splitlines() if ln.strip()]
        assert len(lines) == 1, f"expected 1 log line, got {lines}"
        assert "forged entry" in lines[0], "content kept, just flattened"


def test_claim_marker_wins_exactly_once():
    """The unlink IS the claim, so exactly one caller can win. Two concurrent
    hook processes both pass the freshness stat; without an atomic claim, one
    operator approval would authorize two merges."""
    with tempfile.TemporaryDirectory() as td:
        _, marker = _with_marker(td)
        assert git_protection._claim_marker(marker) is True
        assert git_protection._claim_marker(marker) is False


def test_expired_merge_marker_denies_and_is_removed():
    """A forgotten sentinel must not silently hold the merge boundary open."""
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td, age_seconds=600)
        out, _ = _run_hook_subprocess("gh pr merge 123 --squash", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert not marker.exists(), "expired marker must be reaped"


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
