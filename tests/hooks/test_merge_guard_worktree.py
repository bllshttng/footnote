#!/usr/bin/env python3
"""Tests for the merge/worktree authorization and tokenization gates in
hooks/git-protection.py.

One table test per surface; each row is a distinct branch a regression once
slipped through. The hook is fail-closed, so the deny rows are the contract.
"""

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
HOOK_PATH = REPO_ROOT / "hooks" / "git-protection.py"

_spec = importlib.util.spec_from_file_location("git_protection", HOOK_PATH)
git_protection = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(git_protection)

_MERGE = "gh pr merge"  # kept out of a raw string so this file's own text
#                         never trips a loose gh-pr-merge matcher

_TEST_PR_WORKTREES = {}


@pytest.fixture(autouse=True)
def _stub_pr_worktree_lookup(monkeypatch):
    _TEST_PR_WORKTREES.clear()
    monkeypatch.setattr(
        git_protection,
        "_pr_worktree_root",
        lambda pr: _TEST_PR_WORKTREES.get(str(pr)),
    )

    # The live switch resolves through `fno config get auto_merge.enabled`;
    # answer it from the row's own cwd config so the rows here test the
    # hook's verdict logic, not whether a binary is installed on the runner.
    # Every other command passes through to the real runner.
    real_run = git_protection.subprocess.run

    def _fake_config_get(cmd, **kwargs):
        if cmd[:4] == ["fno", "config", "get", "auto_merge.enabled"]:
            class _R:
                returncode = 1
                stdout = ""
                stderr = ""

            try:
                text = (Path(kwargs["cwd"]) / ".fno" / "config.toml").read_text()
            except (KeyError, OSError):
                text = ""
            for line in text.splitlines():
                if line.strip() == "enabled = true":
                    _R.returncode, _R.stdout = 0, "true\n"
                    break
                if line.strip() == "enabled = false":
                    _R.returncode, _R.stdout = 0, "false\n"
                    break
            return _R()
        return real_run(cmd, **kwargs)

    monkeypatch.setattr(git_protection.subprocess, "run", _fake_config_get)


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
    _TEST_PR_WORKTREES["356"] = wt.resolve()
    return canonical, wt


def _call_in(cwd, fn, *args):
    prev = os.getcwd()
    os.chdir(cwd)
    try:
        return fn(*args)
    finally:
        os.chdir(prev)


# --- worktree state machine: who may authorize a raw merge --------------------

def test_worktree_authorization_rows():
    """Rows: (setup, command, authorized). The guard authorizes via the
    worktree's active session whose artifact records THIS PR; anything else
    fails closed."""
    def active(tmp):
        canonical, wt = _setup_canonical_plus_worktree(tmp)
        _write_state(wt, status="IN_PROGRESS", sid="wt-active-sid")
        _write_external_artifact(wt, "wt-active-sid", pr_number=356)
        return canonical, wt

    def no_session(tmp):
        canonical, wt = _setup_canonical_plus_worktree(tmp)
        _write_state(wt, status="COMPLETE", sid="wt-done-sid")
        return canonical, wt

    def no_artifact(tmp):
        canonical, wt = _setup_canonical_plus_worktree(tmp)
        _write_state(wt, status="IN_PROGRESS", sid="wt-noart-sid")
        return canonical, wt

    def no_approval(tmp):
        canonical, wt = _setup_canonical_plus_worktree(tmp)
        _write_state(wt, status="IN_PROGRESS", sid="wt-noapprove-sid", auto_merge="false")
        _write_external_artifact(wt, "wt-noapprove-sid")
        return canonical, wt

    def cwd_only(tmp):
        canonical = Path(tmp) / "canonical"
        canonical.mkdir()
        _git(canonical, "init", "-q")
        _git(canonical, "config", "user.email", "t@t.t")
        _git(canonical, "config", "user.name", "t")
        (canonical / "README.md").write_text("x\n")
        _git(canonical, "add", "README.md")
        _git(canonical, "commit", "-qm", "init")
        _write_state(canonical, status="IN_PROGRESS", sid="cwd-active-sid")
        _write_external_artifact(canonical, "cwd-active-sid")
        _TEST_PR_WORKTREES["356"] = canonical.resolve()
        return canonical, None

    def mismatch(tmp):
        canonical, wt = _setup_canonical_plus_worktree(tmp)
        _write_state(wt, status="IN_PROGRESS", sid="wt-mismatch-sid")
        _write_external_artifact(wt, "wt-mismatch-sid", pr_number=111)
        return canonical, wt

    def prefer_pr(tmp):
        canonical, wtA = _setup_canonical_plus_worktree(tmp)
        wtB = Path(tmp) / "wtB"
        _git(canonical, "worktree", "add", "-q", "-b", "feature/y", str(wtB))
        _TEST_PR_WORKTREES["999"] = wtB.resolve()
        _write_state(wtA, status="IN_PROGRESS", sid="sid-a")
        _write_external_artifact(wtA, "sid-a", pr_number=356)
        _write_state(wtB, status="IN_PROGRESS", sid="sid-b")
        _write_external_artifact(wtB, "sid-b", pr_number=999)
        return canonical, wtA

    def disarmed(tmp):
        canonical, wt = _setup_canonical_plus_worktree(tmp)
        # x-2270 at the raw path (x-3855): the snapshot must not outlive the
        # operator's disarm.
        _write_state(wt, status="IN_PROGRESS", sid="disarmed-sid", switch="false")
        _write_external_artifact(wt, "disarmed-sid")
        return canonical, wt

    def env_grant(tmp):
        canonical, wt = _setup_canonical_plus_worktree(tmp)
        # x-01b9 at the raw path: a spawn-time grant authorizes without the
        # standing config switch.
        sid = "env-grant-sid"
        d = wt / ".fno"
        d.mkdir(parents=True, exist_ok=True)
        (d / "target-state.md").write_text(
            "---\n"
            f"status: IN_PROGRESS\n"
            f"session_id: {sid}\n"
            "auto_merge_approved: true\n"
            "auto_merge_source: env-target-auto-merge\n"
            "external_review_passed: true\n"
            "---\n"
        )
        (d / "config.toml").write_text("[auto_merge]\nenabled = false\n")
        _write_external_artifact(wt, sid)
        return canonical, wt

    def multi_no_pr(tmp):
        canonical, wtA = _setup_canonical_plus_worktree(tmp)
        wtB = Path(tmp) / "wtB"
        _git(canonical, "worktree", "add", "-q", "-b", "feature/y", str(wtB))
        _write_state(wtA, status="IN_PROGRESS", sid="sid-a")
        _write_external_artifact(wtA, "sid-a", pr_number=356)
        _write_state(wtB, status="IN_PROGRESS", sid="sid-b")
        _write_external_artifact(wtB, "sid-b", pr_number=999)
        return canonical, wtA

    def multi_conflict(tmp):
        # Neither artifact records the requested PR -> no neutral fallback.
        canonical, wtA = _setup_canonical_plus_worktree(tmp)
        wtB = Path(tmp) / "wtC"
        _git(canonical, "worktree", "add", "-q", "-b", "feature/z", str(wtB))
        _write_state(wtA, status="IN_PROGRESS", sid="sid-a")
        _write_external_artifact(wtA, "sid-a", pr_number=111)
        _write_state(wtB, status="IN_PROGRESS", sid="sid-b")
        _write_external_artifact(wtB, "sid-b", pr_number=222)
        return canonical, wtA

    rows = [
        (active, f"{_MERGE} 356 --merge", True),
        (no_session, f"{_MERGE} 356 --merge", False),
        (no_artifact, f"{_MERGE} 356 --merge", False),
        (no_approval, f"{_MERGE} 356 --merge", False),
        (cwd_only, f"{_MERGE} 356 --merge", True),
        (mismatch, f"{_MERGE} 999 --merge", False),
        (prefer_pr, f"{_MERGE} 356 --merge", True),
        (disarmed, f"{_MERGE} 356 --merge", False),
        (env_grant, f"{_MERGE} 356 --merge", True),
        (multi_no_pr, f"{_MERGE} --merge", False),  # ambiguous: fail closed
        (multi_conflict, f"{_MERGE} 356 --merge", False),  # no session owns 356
    ]
    for setup, command, authorized in rows:
        with tempfile.TemporaryDirectory() as rowdir:
            canonical, _wt = setup(rowdir)
            reason = _call_in(canonical, git_protection._check_pr_merge_allowed, command)
            # A truthy reason AUTHORIZES the merge; None blocks.
            assert bool(reason) is authorized, (setup.__name__, command, reason)


def test_parse_merge_pr_forms():
    p = git_protection._parse_merge_pr
    assert p(f"{_MERGE} 356 --merge") == "356"
    assert p(f"{_MERGE} --squash 356") == "356"
    assert p(f"{_MERGE} --auto --delete-branch 356") == "356"
    assert p(f"{_MERGE} https://github.com/o/r/pull/356") == "356"
    assert p(f"{_MERGE} https://github.com/o/r/pull/356/") == "356"
    assert p(_MERGE) is None
    assert p(f"{_MERGE} my-feature-branch") is None
    assert p("cd /x && " + _MERGE + " 42 --merge") == "42"


def test_unreadable_state_path_does_not_raise():
    assert git_protection._parse_active_state(
        Path("/nonexistent/fno-test/target-state.md")) is None


# ---------------------------------------------------------------------------
# Command-position tokenization (the matcher fix)
# ---------------------------------------------------------------------------


def test_find_merge_segment_ignores_quoted_phrase():
    segs = git_protection._command_segments(
        f'fno backlog update x --details "next step; {_MERGE} after review"')
    assert git_protection._find_merge_segment(segs) is None


def test_tokenization_matches_command_position_rows():
    seg = git_protection._command_segments
    fg = git_protection._find_git_segments
    fm = git_protection._find_merge_segment
    assert fm(seg(f"echo hi && {_MERGE} 5")) == f"{_MERGE} 5"
    assert fg(seg("cd /tmp && git push origin main")) == ["git push origin main"]
    # shlex eats newlines in whitespace_split mode; the physical-line split is
    # what makes line 2 its own segment.
    assert fm(seg(f"git status\n{_MERGE} 356 --squash")) is not None
    assert fg(seg("git log | grep foo")) == ["git log"]
    # Wrapper/assignment/path/subshell prefixes must not hide the merge verb.
    for cmd in (
        f"GH_TOKEN=x {_MERGE} 356 --squash",
        f"env {_MERGE} 356",
        f"sudo {_MERGE} 356",
        "/usr/bin/gh pr merge 356",
        "(gh pr merge 356)",
    ):
        assert fm(seg(cmd)) is not None, cmd
    for cmd in (
        "GIT_DIR=/x git push origin main",
        "sudo git push origin main",
        "/usr/bin/git push origin main",
    ):
        assert fg(seg(cmd)), cmd
    # A backslash continuation joins two physical lines into one command.
    assert fg(seg("git commit \\\n  --no-verify -m x")) == ["git commit --no-verify -m x"]
    assert fg(seg("git push \\\n  origin main")) == ["git push origin main"]
    assert fm(seg(f"{_MERGE} \\\n  5")) == f"{_MERGE} 5"
    # A mid-token continuation rejoins (shell semantics: removed, not spaced).
    assert fg(seg("git pu\\\nsh origin main")) == ["git push origin main"]
    try:
        seg(f'{_MERGE} 5 --body "unclosed')
    except ValueError:
        pass
    else:
        raise AssertionError("expected ValueError on unbalanced quote")


# ---------------------------------------------------------------------------
# Subprocess behavior: markers, approvals, evasion rows
# ---------------------------------------------------------------------------

def _run_hook_subprocess(command, fno_home, cwd=None, extra_env=None):
    env = dict(os.environ, FNO_HOME=str(fno_home))
    # Give the fail-closed hold veto an explicit unheld answer while preserving
    # the fail-open behavior of unrelated missing probe verbs.
    bin_dir = Path(fno_home).parent / "hook-bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    fno = bin_dir / "fno"
    fno.write_text(
        '#!/usr/bin/env bash\n'
        '[[ "$1 $2 $3" == "do pr hold-check" ]] && exit 0\n'
        'exit 1\n'
    )
    fno.chmod(0o755)
    fno_agents = bin_dir / "fno-agents"
    fno_agents.write_text(
        '#!/usr/bin/env bash\n'
        'cat >/dev/null\n'
        '# The hold reader also asks the one roster (the self-review floor\n'
        '# enumerates the verbless harnesses through it); serve it beside the\n'
        '# claim door the way the real binary does.\n'
        'if [[ "$1" == "harness-roster" ]]; then\n'
        '  printf \'{"known":["claude","codex","gemini","agy","opencode","pi",'
        '"hermes","openclaw","cursor-agent","grok","zcode"]}\\n\'\n'
        '  exit 0\n'
        'fi\n'
        'printf \'{"worktree":"%s"}\\n\' "$PWD"\n'
    )
    fno_agents.chmod(0o755)
    env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"
    env["FNO_AGENTS_BIN"] = str(fno_agents)
    # $FNO_CONFIG makes one temp settings file the only candidate, so any
    # settings read inside the hook resolves in the sandbox, not the machine.
    config = Path(fno_home).parent / "hook-settings.yaml"
    config.write_text(f"state_dir: {fno_home}\n")
    env["FNO_CONFIG"] = str(config)
    env.update(extra_env or {})
    payload = json.dumps({"tool_name": "Bash", "tool_input": {"command": command}})
    p = subprocess.run([sys.executable, str(HOOK_PATH)], input=payload,
                       capture_output=True, text=True, env=env, cwd=cwd)
    return p.stdout, p.returncode


def test_state_writes_land_under_fno_home():
    """A blocked protected push writes state/git-protection.json under FNO_HOME
    and creates nothing under a harness state dir in the sandbox (AC2-HP)."""
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        out, _ = _run_hook_subprocess("git push origin main", fno)
        assert '"permissionDecision": "deny"' in out
        assert (fno / "state" / "git-protection.json").exists()
        assert not (Path(td) / ".claude").exists()


def _with_marker(td, name="merge-gate.disabled", age_seconds=0):
    fno = Path(td) / ".fno"
    fno.mkdir(parents=True, exist_ok=True)
    marker = fno / name
    marker.write_text("")
    if age_seconds:
        past = time.time() - age_seconds
        os.utime(marker, (past, past))
    return fno, marker


_BT = chr(96)  # backtick, kept out of the f-strings below for readability


def test_merge_marker_never_opens_the_branch_or_no_verify_gates():
    # Operator policy 2026-08-07: auto-merge after the gates pass is fine;
    # disabling main-push protection never is. The marker used to sit ahead of
    # every gate as an unconditional exit(0).
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td)
        rows = [
            "git push origin main",
            # --no-verify skips .git/hooks/pre-push, which IS the branch guard.
            "git push --no-verify origin main",
            "git commit --no-verify -m x",
            # A merge decision covers the merge segment only; a compound must
            # not ride the merge allow past the branch gate.
            "gh pr merge 123 --squash && git push origin main",
        ]
        for cmd in rows:
            marker.write_text("")
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd
            assert marker.exists(), f"a denied command must not consume: {cmd}"


def test_one_approval_authorizes_one_action():
    # A deny anywhere outranks an allow anywhere, in EITHER segment order; the
    # loop used to short-circuit on the first allow.
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        for cmd in ("git commit --no-verify -m x && git push origin main",
                    "git push origin main && git commit --no-verify -m x"):
            flag.write_text("")
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd
            assert flag.exists(), f"denied command must not consume: {cmd}"

    # Mixing an approved --no-verify segment with a merge is refused: one
    # approval authorizes one action, and a merge denial must not burn the
    # operator's --no-verify approval.
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td)
        flag = fno / "approve_no_verify.flag"
        flag.write_text("")
        out, _ = _run_hook_subprocess(
            "gh pr merge 1 --squash && git commit --no-verify -m x", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert flag.exists()


def test_marker_and_flag_are_single_use():
    # The unlink IS the claim, so exactly one caller can win.
    with tempfile.TemporaryDirectory() as td:
        _, marker = _with_marker(td)
        assert git_protection._claim_marker(marker) is True
        assert git_protection._claim_marker(marker) is False

    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td)
        out1, _ = _run_hook_subprocess("gh pr merge 123 --squash", fno, cwd=td)
        assert '"permissionDecision": "allow"' in out1
        assert not marker.exists(), "marker must be single-use"
        log = fno / "logs" / "merge-gate-overrides.log"
        assert log.exists() and "123" in log.read_text()
        out2, _ = _run_hook_subprocess("gh pr merge 123 --squash", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out2

    # One consumed override = exactly one log line; a newline forges nothing.
    with tempfile.TemporaryDirectory() as td:
        fno, _ = _with_marker(td)
        out, _ = _run_hook_subprocess(
            'gh pr merge 123 --body "x\n2099-01-01 forged entry"', fno, cwd=td)
        assert '"permissionDecision": "allow"' in out
        lines = [ln for ln in (fno / "logs" / "merge-gate-overrides.log")
                 .read_text().splitlines() if ln.strip()]
        assert len(lines) == 1
        assert "forged entry" in lines[0]

    # A forgotten sentinel must not silently hold the merge boundary open.
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td, age_seconds=600)
        out, _ = _run_hook_subprocess("gh pr merge 123 --squash", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert not marker.exists(), "expired marker must be reaped"

    # Approved --no-verify allows and consumes the flag; the missing-flag race
    # denies without crashing (AC1-FR: unlink(missing_ok=True)).
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


def test_evasion_rows_are_denied():
    """Rows: one token away from a bare push/merge. Each is a distinct
    construct that once hid the verb from both gates."""
    deny_rows = [
        "! git push origin main",
        "if git push origin main; then true; fi",
        "{ git push origin main; }",
        "while git push origin main; do true; done",
        "until git push origin main; do true; done",
        "for x in a; do git push origin main; done",
        "if gh pr merge 42 --merge; then true; fi",
        "true |& git push origin main",
        "case x in *) git push origin main;; esac",
        "case x in *) gh pr merge 42 --admin;; esac",
        "eval git push origin main",
        "coproc git push origin main",
        'eval "git push origin main"',
        'bash -c "git push origin main"',
        'sh -c "git push origin main"',
        # A wrapper that takes its own option pushed the verb past argv[0].
        "bash -lc 'git push origin main'",
        "sh -cx 'git push origin main'",
        "bash -lc 'gh pr merge 42'",
        "env -i git push origin main",
        "nice -n 5 git push origin main",
        "sudo -u x git push origin main",
        "timeout 10 git push origin main",
        # `GIT`/`ENV`/`SUDO` resolve on a case-insensitive filesystem.
        "GIT push origin main",
        "Git push origin main",
        "ENV git push origin main",
        "SUDO git push origin main",
        # Global options must not hide the subcommand; -c hooksPath is a
        # --no-verify by another name (it disables .git/hooks/pre-push).
        "git -C /repo push origin main",
        "git --no-pager push origin main",
        "git -c core.hooksPath=/dev/null push origin main",
        "git -c core.hooksPath=/dev/null commit -m x",
        "git config core.hooksPath /dev/null",
        # Only the `feature:main` form was once normalized; these reach main.
        "git push origin +main",
        "git push origin refs/heads/main",
        "git push origin +refs/heads/main",
        "git push --all origin",
        "git push --mirror origin",
        # On the unbalanced-quote fallback the "segment" is the WHOLE string,
        # so a positional allowlist read the FIRST command's subcommand.
        "git status && git push origin main 'unbal",
        'git log --oneline && git push origin main --message "unbal',
    ]
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        for cmd in deny_rows:
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd

    # Allowed controls each run in a FRESH sandbox: an allowed push stamps the
    # branch, and a second push in the same sandbox would read as debounced.
    for cmd in ("timeout 10 git push origin feature/x",
                "git -C /repo push origin feature/x",
                "git config user.name"):
        with tempfile.TemporaryDirectory() as td:
            fno = Path(td) / ".fno"
            fno.mkdir(parents=True)
            out, rc = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' not in out and rc == 0, cmd


def test_hooks_path_and_message_text_rows():
    # The hooks door must not open the branch door, in either direction: with
    # the approval flag present, a hooksPath override on a push to main
    # returned allow - the inversion branch-gate-first ordering exists to stop.
    with tempfile.TemporaryDirectory() as td:
        fno, _ = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            "git -c core.hooksPath=/dev/null push origin main", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out

    # A substring scan over quote-stripped text refused a commit whose MESSAGE
    # named core.hooksPath.
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        out, rc = _run_hook_subprocess(
            'git commit -m "docs: explain core.hooksPath guard"', fno, cwd=td)
        assert '"permissionDecision": "deny"' not in out and rc == 0

    # Segments arrive shlex-rejoined with quotes stripped, so a regex allowlist
    # read argument text as the command. The check is positional, so message
    # text cannot decide either way.
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        for cmd in ('git commit -m "fix: block git push origin main"',
                    'git log --grep "git push origin main"'):
            out, rc = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' not in out and rc == 0, cmd
        out2, _ = _run_hook_subprocess(
            'git commit --no-verify -m "see git log"', fno, cwd=td)
        assert '"permissionDecision": "allow"' in out2
        assert not flag.exists(), "the approval was actually consumed"

    # The runner's quoted argument is re-tokenized so the inner verb is gated.
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


def test_authorization_disqualified_by_extra_capability_rows():
    # Nothing can be counted on the unparseable fallback, so nothing may be
    # authorized there either.
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            "git commit --no-verify -m 'it's ready' && rm -rf /tmp/zzz",
            fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert flag.exists()

    # An authorization covers the whole Bash call, so a `>` rides it into an
    # arbitrary file overwrite no gate inspects.
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            "git commit --no-verify -m ok > /tmp/gp-test-log", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert flag.exists()

    # A `$(...)` body is re-segmented and trips the count, but backticks are
    # skipped and `<(` is not a separator: both must stay disqualified.
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td)
        for cmd in (f'gh pr merge 12 --squash --body "{_BT}id{_BT}"',
                    "gh pr merge 12 --squash --body-file <(id)"):
            out, _ = _run_hook_subprocess(cmd, fno, cwd=td)
            assert '"permissionDecision": "deny"' in out, cmd
            assert marker.exists(), cmd


def test_lone_command_rule_rows():
    # The rule applies to every authorizing path: the sibling here is not a git
    # segment, yet the approval's allow would have covered the whole call.
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            "git commit --no-verify -m x && gh api -X PATCH "
            "repos/o/r/git/refs/heads/main", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert flag.exists()

    # A leading `cd` is deliberately not carved out: an allow covers a prefix
    # exactly as it covers a suffix.
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        out, _ = _run_hook_subprocess(
            "gh pr merge 1 --squash && gh api -X PATCH "
            "repos/o/r/git/refs/heads/main", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out

    # The protected-branch gate outranks the --no-verify approval: ONE segment,
    # so no cross-segment rule can catch it - the evaluator once returned allow
    # without ever reaching the branch check.
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess("git push --no-verify origin main",
                                      fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert flag.exists(), "a denied push must not consume the approval"

    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            "git commit --no-verify -m a && git commit --no-verify -m b",
            fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert flag.exists()

    # A marker-authorized merge would approve whatever rides along.
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td)
        out, _ = _run_hook_subprocess(
            "gh pr merge 1 --squash && gh api -X PATCH "
            "repos/o/r/git/refs/heads/main", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert marker.exists(), "a refused override must not be spent"

    # `gh pr merge 1 && gh pr merge 2` rode one consume; only the first
    # reached the audit log.
    with tempfile.TemporaryDirectory() as td:
        fno, marker = _with_marker(td)
        out, _ = _run_hook_subprocess(
            "gh pr merge 1 --squash && gh pr merge 2 --squash", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert marker.exists()


def test_fail_closed_state_rows():
    # save_state runs first on every protected push; an unguarded OSError
    # would exit non-zero, which a PreToolUse hook treats as non-blocking - a
    # crash here fails OPEN.
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        (fno / "state" / "git-protection.json").mkdir(parents=True)   # a directory where a file goes
        out, _ = _run_hook_subprocess("git push origin main", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert "Traceback" not in out

    # The trail is what justifies having an override: an unwritable log refuses
    # rather than allowing unrecorded.
    with tempfile.TemporaryDirectory() as td:
        fno, _ = _with_marker(td)
        (fno / "logs" / "merge-gate-overrides.log").mkdir(parents=True)
        out, _ = _run_hook_subprocess("gh pr merge 9 --squash", fno, cwd=td)
        assert '"permissionDecision": "deny"' in out

    # A stale git-protection.disabled from before the rename must no longer
    # bypass anything. The rename is the migration: fail-safe, no cleanup.
    with tempfile.TemporaryDirectory() as td:
        fno, _ = _with_marker(td, name="git-protection.disabled")
        out, _ = _run_hook_subprocess("git push origin main", fno)
        assert '"permissionDecision": "deny"' in out


def test_two_factor_merge_rows():
    # An apostrophe raises in shlex; the fallback merge must be refused by the
    # MERGE gate, naming the real reason, not the lone-command rule.
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        out, _ = _run_hook_subprocess(
            f'{_MERGE} 12 --body "it\'s ready"', fno, cwd=td)
        assert '"permissionDecision": "deny"' in out
        assert "two-factor check failed" in out
        assert "one approval cannot authorize" not in out


def test_allowed_forms_rows():
    # `2>&1` duplicates a descriptor and writes no file.
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess("git commit --no-verify -m ok 2>&1",
                                      fno, cwd=td)
        assert '"permissionDecision": "allow"' in out

    # The opener scan walked BODY lines too, so a message that merely
    # mentioned <<EOF broke the recommended escape.
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            "git commit --no-verify -F - <<'EOF'\nfix <<EOF parsing\nEOF",
            fno, cwd=td)
        assert '"permissionDecision": "allow"' in out

    # `<<` inside an ARGUMENT is not a heredoc opener.
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            'git commit --no-verify -m "shift << 2"', fno, cwd=td)
        assert '"permissionDecision": "allow"' in out

    # The refusal message recommends `-F -` with a quoted heredoc; a markdown
    # code span in the message must survive. An UNQUOTED delimiter expands.
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

    # The long-message form the refusal points at must actually work, and the
    # approval is consumed on the allow.
    with tempfile.TemporaryDirectory() as td:
        fno, flag = _with_marker(td, name="approve_no_verify.flag")
        out, _ = _run_hook_subprocess(
            "git commit --no-verify -F - <<'EOF'\nmsg\nEOF", fno, cwd=td)
        assert '"permissionDecision": "allow"' in out
        assert not flag.exists()

    # "One approval must not open the other door" holds in BOTH directions;
    # the branch bypass itself still works for a plain push.
    with tempfile.TemporaryDirectory() as td:
        fno = Path(td) / ".fno"
        fno.mkdir(parents=True)
        env = {"CLAUDE_RECENT_USER_MESSAGE": "Push to Main"}
        out, _ = _run_hook_subprocess("git push --no-verify origin main",
                                      fno, cwd=td, extra_env=env)
        assert '"permissionDecision": "deny"' in out
        out2, rc2 = _run_hook_subprocess("git push origin main", fno, cwd=td,
                                         extra_env=env)
        assert '"permissionDecision": "deny"' not in out2 and rc2 == 0


if __name__ == "__main__":
    raise SystemExit("run with pytest")
