#!/usr/bin/env python3
"""Tests for the push/merge protection in hooks/git-protection.py.

Run: python3 tests/hooks/test_git_protection_push.py
 or: pytest tests/hooks/test_git_protection_push.py

One table test per surface (explicit-destination parse, push debounce,
command segmentation), one row per distinct branch. The hook is fail-closed;
every row is a branch a regression once slipped through.
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


def _run_hook(command):
    """Drive the real hook end to end and return its decision dict ({} on
    allow-by-silence). HOME/FNO_HOME are sandboxed so a host opt-out marker or
    approval flag can never turn a deny into a pass."""
    payload = json.dumps({"tool_name": "Bash", "tool_input": {"command": command}})
    with tempfile.TemporaryDirectory() as tmp:
        env = {**os.environ, "HOME": tmp, "FNO_HOME": str(Path(tmp) / ".fno")}
        proc = subprocess.run([sys.executable, str(HOOK_PATH)], input=payload,
                              capture_output=True, text=True, env=env, cwd=tmp)
    return json.loads(proc.stdout).get("hookSpecificOutput", {}) if proc.stdout.strip() else {}


def _on_main(monkeypatched_branch="main"):
    git_protection.get_current_branch = lambda: monkeypatched_branch


# --- explicit-destination parse: cwd branch is irrelevant ----------------------
# Regression: is_push_to_protected_branch() ran the current-branch check
# unconditionally, blocking `git push origin feature/x` from a cwd on main.
# The fix returns early once an explicit, non-protected destination is parsed.
# The early return must not fire on an ambiguous single-token or HEAD push.

def test_explicit_destination_rows():
    rows = [
        # (cwd branch, command, expect_blocked, expect_branch)
        ("main", "git push origin feature/foo", False, None),
        ("feature/x", "git push origin main", True, "main"),
        ("main", "git push", True, "main"),
        ("feature/x", "git push origin feature/x:main", True, "main"),
        # extract_branch_from_push returns the REMOTE or HEAD as if it were a
        # branch, which would otherwise bypass protection on main.
        ("main", "git push origin", True, "main"),
        ("main", "git push --force origin", True, "main"),
        ("main", "git push origin HEAD", True, "main"),
        ("main", "git push origin @", True, "main"),
        ("main", "git push -u origin feature/x", False, None),
        # --force-with-lease carries an =<ref> value that must be stripped
        # whole, else the leftover token shifts the positional parse.
        ("feature/x", "git push --force-with-lease origin feature/x", False, None),
        ("feature/x", "git push --force-with-lease origin main", True, "main"),
        ("feature/x", "git push --force-with-lease=origin/feature origin main", True, "main"),
        ("feature/x", "git push --force-with-lease=origin/main origin feature/x", False, None),
        ("feature/x", "git push --force origin main", True, "main"),
        ("feature/x", "git push --force-with-lease=refs/heads/main origin main", True, "main"),
    ]
    for cwd, command, blocked, branch in rows:
        _on_main(cwd)
        assert git_protection.is_push_to_protected_branch(command) == (blocked, branch), command


# ===========================================================================
# Push debounce: the stamp covers registration, then the real fno probe covers
# a check that is already visible. Probe failures always allow the push.
# ===========================================================================

class _DebounceHarness:
    def __init__(self, tmp_path, monkeypatch):
        monkeypatch.setattr(git_protection, "PUSH_STAMP_DIR", tmp_path / "push-stamps")
        self.bypass_events = []
        monkeypatch.setattr(
            git_protection, "_emit_push_bypass_event", self.bypass_events.append
        )
        monkeypatch.delenv("FNO_PUSH_NOW", raising=False)


def _old_stamp():
    stamp = git_protection._push_stamp_path("feature/x")
    old = time.time() - git_protection.PUSH_DEBOUNCE_SECONDS - 1
    os.utime(stamp, (old, old))


def _fake_fno(tmp_path, monkeypatch, body):
    bindir = tmp_path / "bin"
    bindir.mkdir(exist_ok=True)
    fake = bindir / "fno"
    fake.write_text("#!/bin/sh\n" + body)
    fake.chmod(0o755)
    monkeypatch.setenv("PATH", f"{bindir}:{os.environ.get('PATH', '')}")


def test_debounce_bypass_and_fresh_stamp(tmp_path, monkeypatch):
    h = _DebounceHarness(tmp_path, monkeypatch)
    monkeypatch.setenv("FNO_PUSH_NOW", "1")
    monkeypatch.setattr(
        git_protection, "_read_in_flight", lambda branch: (_ for _ in ()).throw(AssertionError())
    )
    assert git_protection.push_debounce_refusal("git push origin feature/x", "feature/x") is None
    assert h.bypass_events == ["feature/x"]

    _DebounceHarness(tmp_path, monkeypatch)
    git_protection._stamp_push("feature/x")
    monkeypatch.setattr(
        git_protection, "_read_in_flight", lambda branch: (_ for _ in ()).throw(AssertionError())
    )
    reason = git_protection.push_debounce_refusal("git push origin feature/x", "feature/x")
    assert reason is not None
    assert "pushed 0s ago" in reason


def test_debounce_probe_outcome_rows(tmp_path, monkeypatch):
    # (probe script body, script exit, refusal expected)
    rows = [
        # Probe true: refusal names the supersede doors.
        ('echo \'{"in_flight": true, "check": "rust e2e concurrency stress (20 trials)", "job": "106470248875", "head": "f4d1732d"}\'\nexit 2\n', True),
        # Probe false, usage error, timeout: always allow and stamp.
        ('echo \'{"in_flight": false}\'\nexit 0\n', False),
        ('echo usage error >&2\nexit 2\n', False),
        ("sleep 1\n", False),
    ]
    for probe_body, refuse in rows:
        _DebounceHarness(tmp_path, monkeypatch)
        git_protection._stamp_push("feature/x")
        _old_stamp()
        if probe_body == "sleep 1\n":
            monkeypatch.setattr(git_protection, "_PUSH_PROBE_TIMEOUT", 0.01)
        else:
            _fake_fno(tmp_path, monkeypatch, probe_body)
        reason = git_protection.push_debounce_refusal("git push origin feature/x", "feature/x")
        assert (reason is not None) is refuse, probe_body
    # The true-probe refusal names every supersede door.
    _DebounceHarness(tmp_path, monkeypatch)
    git_protection._stamp_push("feature/x")
    _old_stamp()
    _fake_fno(
        tmp_path,
        monkeypatch,
        'echo \'{"in_flight": true, "check": "rust e2e concurrency stress (20 trials)", "job": "106470248875", "head": "f4d1732d"}\'\nexit 2\n',
    )
    reason = git_protection.push_debounce_refusal("git push origin feature/x", "feature/x")
    for needle in ("rust e2e concurrency stress", "106470248875", "fno do pr wait", "--force-ci-cancel", "FNO_PUSH_NOW=1"):
        assert needle in reason


# ===========================================================================
# Command segmentation: heredoc bodies and quoted arguments are CONTENT, not
# command positions; $( ) bodies ARE commands even inside double quotes.
# ===========================================================================

def _git_segments(cmd):
    return git_protection._find_git_segments(git_protection._command_segments(cmd))


def _merge_segment(cmd):
    return git_protection._find_merge_segment(git_protection._command_segments(cmd))


def test_segmentation_content_is_not_command_rows():
    rows = [
        "python3 - <<'PY'\nprint('eg: cd /tmp && git push origin main')\nPY",
        "cat <<EOF\nnotes: git push --force origin main is blocked\nEOF",
        # <<- strips leading tabs from the terminator; the body is still content.
        "cat <<-EOF\n\tsee: cd /tmp && git push origin main\n\tEOF",
        'echo "doc: run git push --force origin main to test"',
        'fno backlog update x --details "see gh pr merge notes"',
        # A newline inside an open quote is part of one argument.
        'fno agents mail send x "line one\ngh pr merge --auto is the bug\nline three"',
        'fno agents mail send x "intro\ngit push --force origin main is blocked\nend"',
        "fno agents mail send x 'intro\ngit push origin main\nend'",
        # The \" is data; the argument stays open.
        'fno agents mail send x "he said \\"hi\\"\ngit push origin main\nend"',
        # `$(cat <<'BODY' ... BODY)"` inside double quotes: a quoted `<<` is
        # data, so the body never earns the heredoc exemption.
        (
            'gh pr create --title "t" --body "$(cat <<\'BODY\'\n'
            "| real `gh pr merge` | deny |\n"
            "prose mentioning gh pr merge\n"
            "BODY\n"
            ')"'
        ),
        # No expansion in single quotes: inert prose.
        'fno agents mail send x \'see "$(git push origin main)"\'',
    ]
    for cmd in rows:
        assert _git_segments(cmd) == [], cmd.splitlines()[0]
        assert _merge_segment(cmd) is None, cmd.splitlines()[0]


def test_segmentation_real_commands_are_caught_rows():
    rows = [
        "echo hi && git push origin main",
        "cat <<EOF\nbody\nEOF\ngit push origin main",
        "git push \\\norigin main",
        "cat <<EOF\ngit push origin main",  # unterminated heredoc fails closed
        "cd /tmp && git push origin main",
        # A << inside quotes is data, not an opener: the next line is judged.
        'echo "use <<EOF here"\ngit push origin main',
        "git push origin main <<EOF\nbody\nEOF",
        # `# <<EOF` is a comment, not an opener.
        "echo ok # <<EOF\ngit push --force origin main\nEOF",
        # An UNQUOTED newline still splits.
        'echo "safe prose"\ngit push origin main',
        'echo "prose about git push"; git push origin main',
        'X="$(gh pr merge 1 --auto\n)"',  # $( ) bodies are commands in quotes
        'echo "$(git push origin main)"',
    ]
    for cmd in rows:
        assert _git_segments(cmd) or _merge_segment(cmd), cmd.splitlines()[0]


def test_segmentation_exact_shapes():
    # Quote tracking pauses inside a heredoc body: one apostrophe in prose
    # otherwise swallows the terminator's newline.
    cmd = "cat <<EOF\nthis doesn't apply cleanly\nEOF\necho after"
    assert git_protection._command_segments(cmd) == [
        ["cat", "<<", "EOF"], ["echo", "after"]]
    # The quote never closes: shlex must raise, not parse as safe.
    try:
        git_protection._command_segments('echo "intro\ngit push origin main')
    except ValueError:
        pass
    else:
        raise AssertionError("unterminated quote must raise, not parse as safe")


def test_fail_closed_end_to_end_rows():
    # The ValueError fallback must lean deny for git commands, not fail open on
    # a literal startswith("git"). Drives the real hook end to end: asserting
    # the predicate in isolation would pass even if main() reverted.
    rows = [
        'echo "intro\ngit push origin main',
        "cat <<EOF\nit doesn't matter\ngit push origin main",
    ]
    for cmd in rows:
        out = _run_hook(cmd)
        assert out.get("permissionDecision") == "deny", cmd


def test_git_grep_carrying_a_guarded_pattern_is_an_allowlisted_read():
    # git grep cannot write; commit and log got this fix for the same class,
    # grep was simply missed.
    out = _run_hook("git grep -n -E 'git push' -- .")
    assert out.get("permissionDecision") != "deny", out
    assert git_protection.is_allowed_git_command("git grep -n -E 'git push' -- .")
    _on_main("feature/x")
    assert _git_segments("git push origin main")
    assert git_protection.is_push_to_protected_branch(
        "git push origin main") == (True, "main")


if __name__ == "__main__":
    raise SystemExit("run with pytest")
