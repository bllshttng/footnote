#!/usr/bin/env python3
"""Tests for is_push_to_protected_branch's explicit-destination fallthrough.

Run: python3 tests/hooks/test_git_protection_push.py
 or: pytest tests/hooks/test_git_protection_push.py

Regression: is_push_to_protected_branch() ran the current-branch
check unconditionally, so `git push origin feature/x` from a session whose cwd
HEAD is `main` (the normal background /target case: cwd pinned to the canonical
checkout while the branch lives in a worktree) was wrongly blocked as a push to
main. The fix returns early once an explicit, non-protected destination is
parsed. get_current_branch is monkeypatched to "main" to simulate that cwd.
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


def test_feature_push_from_cwd_on_main_is_allowed():
    _on_main("main")
    assert git_protection.is_push_to_protected_branch(
        "git push origin feature/foo") == (False, None)


def test_explicit_push_to_main_still_blocked():
    _on_main("feature/x")  # cwd branch is irrelevant; explicit dest is main
    assert git_protection.is_push_to_protected_branch(
        "git push origin main") == (True, "main")


def test_bare_push_on_protected_branch_still_blocked():
    _on_main("main")
    assert git_protection.is_push_to_protected_branch("git push") == (True, "main")


def test_refspec_to_protected_dest_still_blocked():
    _on_main("feature/x")
    assert git_protection.is_push_to_protected_branch(
        "git push origin feature/x:main") == (True, "main")


# The early return must not fire on an ambiguous single-token or current-branch
# push: `extract_branch_from_push` returns the REMOTE ("origin") or "HEAD" as if
# it were a branch, which would otherwise bypass protection on `main`.

def test_remote_only_push_on_main_still_blocked():
    _on_main("main")
    assert git_protection.is_push_to_protected_branch(
        "git push origin") == (True, "main")


def test_force_remote_only_push_on_main_still_blocked():
    _on_main("main")
    assert git_protection.is_push_to_protected_branch(
        "git push --force origin") == (True, "main")


def test_push_head_on_main_still_blocked():
    _on_main("main")
    assert git_protection.is_push_to_protected_branch(
        "git push origin HEAD") == (True, "main")


def test_push_at_alias_on_main_still_blocked():
    _on_main("main")
    assert git_protection.is_push_to_protected_branch(
        "git push origin @") == (True, "main")


def test_upstream_flag_feature_push_still_allowed():
    _on_main("main")
    assert git_protection.is_push_to_protected_branch(
        "git push -u origin feature/x") == (False, None)


# --force-with-lease carries an =<ref> value that must be stripped whole, else
# the leftover token shifts the positional parse and a force-with-lease to a
# protected branch slips through the destination check.

def test_force_with_lease_to_feature_allowed():
    _on_main("feature/x")
    assert git_protection.is_push_to_protected_branch(
        "git push --force-with-lease origin feature/x") == (False, None)


def test_force_with_lease_to_main_still_blocked():
    _on_main("feature/x")
    assert git_protection.is_push_to_protected_branch(
        "git push --force-with-lease origin main") == (True, "main")


def test_force_with_lease_ref_value_to_main_still_blocked():
    _on_main("feature/x")
    assert git_protection.is_push_to_protected_branch(
        "git push --force-with-lease=origin/feature origin main") == (True, "main")


def test_force_with_lease_ref_value_to_feature_allowed():
    _on_main("feature/x")
    assert git_protection.is_push_to_protected_branch(
        "git push --force-with-lease=origin/main origin feature/x") == (False, None)


# ===========================================================================
# Defect B: heredoc bodies are CONTENT, not command positions.
#
# _command_segments used to split on physical lines with no heredoc awareness,
# so every line of a <<DELIM ... DELIM body was judged as a potential command.
# A doc/test/filing command whose body quoted a guarded git invocation was
# denied, with the refusal echoing a fragment of prose. These exercise the
# segmentation layer directly via the same importlib load above; main()'s
# PreToolUse path is never invoked, so they verify behavior without the live
# guard in the loop.
# ===========================================================================

def _git_segments(cmd):
    return git_protection._find_git_segments(git_protection._command_segments(cmd))


def _merge_segment(cmd):
    return git_protection._find_merge_segment(git_protection._command_segments(cmd))


def test_heredoc_body_with_quoted_push_is_not_a_command():
    assert _git_segments(
        "python3 - <<'PY'\nprint('eg: cd /tmp && git push origin main')\nPY") == []


def test_heredoc_body_unquoted_delimiter_is_not_a_command():
    assert _git_segments(
        "cat <<EOF\nnotes: git push --force origin main is blocked\nEOF") == []


def test_heredoc_dash_delimiter_tab_terminator_is_not_a_command():
    # <<- strips leading tabs from the terminator; the body line is still content.
    assert _git_segments(
        "cat <<-EOF\n\tsee: cd /tmp && git push origin main\n\tEOF") == []


def test_real_push_after_separator_still_caught():
    assert _git_segments("echo hi && git push origin main")


def test_real_push_after_heredoc_close_still_caught():
    assert _git_segments("cat <<EOF\nbody\nEOF\ngit push origin main")


def test_real_push_on_continuation_line_still_caught():
    assert _git_segments("git push \\\norigin main")


def test_real_merge_invocation_still_caught():
    assert _merge_segment("gh pr merge 123") is not None


def test_unterminated_heredoc_fails_closed():
    # No closing delimiter: the body exemption must NOT apply, so a guarded
    # invocation in the unterminated body is still caught (deny), not hidden.
    assert _git_segments("cat <<EOF\ngit push origin main")


def test_merge_phrase_mention_in_quoted_arg_allowed():
    assert _merge_segment(
        'fno backlog update x --details "see gh pr merge notes"') is None


def test_push_phrase_mention_in_quoted_arg_allowed():
    assert _git_segments(
        'echo "doc: run git push --force origin main to test"') == []


def test_force_push_to_main_still_protected():
    _on_main("feature/x")
    assert _git_segments("git push --force origin main")
    assert git_protection.is_push_to_protected_branch(
        "git push --force origin main") == (True, "main")


def test_force_with_lease_ref_push_to_main_still_protected():
    _on_main("feature/x")
    assert git_protection.is_push_to_protected_branch(
        "git push --force-with-lease=refs/heads/main origin main") == (True, "main")


def test_compound_cd_then_push_still_caught():
    assert _git_segments("cd /tmp && git push origin main")


def test_quoted_heredoc_opener_does_not_swallow_next_line():
    # A << inside quotes is data, not an opener: the following real command
    # must still be judged.
    assert _git_segments('echo "use <<EOF here"\ngit push origin main')


def test_opener_line_git_prefix_still_caught():
    # The opener line's own command prefix is segmented normally.
    assert _git_segments("git push origin main <<EOF\nbody\nEOF")


def test_heredoc_opener_after_shell_comment_is_ignored():
    # `# <<EOF` is a comment, not an opener; the shell executes the following
    # push, so it must be judged as a real command, not hidden as heredoc body.
    assert _git_segments("echo ok # <<EOF\ngit push --force origin main\nEOF")


# --- multi-line QUOTED arguments are content, not command positions ----------
# A newline inside an open quote is part of one argument, so splitting on it
# handed shlex a fragment with an unbalanced quote; that raises, and the caller
# falls back to a whole-command regex that matches the phrase anywhere. Any
# message whose BODY quoted a guarded invocation was refused - including a
# worker's review report ABOUT merge behaviour (observed live 2026-08-06).


def test_multiline_quoted_body_mentioning_merge_is_not_a_command():
    assert _merge_segment(
        'fno mail send x "line one\ngh pr merge --auto is the bug\nline three"') is None


def test_multiline_quoted_body_mentioning_push_is_not_a_command():
    assert _git_segments(
        'fno mail send x "intro\ngit push --force origin main is blocked\nend"') == []


def test_multiline_single_quoted_body_is_not_a_command():
    assert _git_segments(
        "fno mail send x 'intro\ngit push origin main\nend'") == []


def test_escaped_quote_inside_multiline_body_does_not_end_the_quote():
    # The \" is data; the argument stays open, so the push line is still content.
    assert _git_segments(
        'fno mail send x "he said \\"hi\\"\ngit push origin main\nend"') == []


def test_real_push_on_next_line_outside_quotes_still_caught():
    # An UNQUOTED newline still splits, so a genuine two-liner is still judged.
    assert _git_segments('echo "safe prose"\ngit push origin main')


def test_real_push_after_closed_quote_same_line_still_caught():
    assert _git_segments('echo "prose about git push"; git push origin main')


def test_unterminated_quote_hiding_a_push_still_fails_closed():
    # The quote never closes, so the whole command stays one line and shlex
    # raises - the caller's deny-leaning whole-command fallback, unchanged.
    try:
        git_protection._command_segments('echo "intro\ngit push origin main')
    except ValueError:
        return
    raise AssertionError("unterminated quote must raise, not parse as safe")


def test_heredoc_inside_command_substitution_body_is_not_a_command():
    # `gh pr create --body "$(cat <<'BODY' ... BODY )"` opens the heredoc INSIDE
    # a double-quoted $( ), and _find_heredoc_opener deliberately treats a quoted
    # `<<` as data - so the body never earns the heredoc exemption and its lines
    # were judged as commands. This shape blocked this fix's own pull request.
    cmd = (
        'gh pr create --title "t" --body "$(cat <<\'BODY\'\n'
        "| real `gh pr merge` | deny |\n"
        "prose mentioning gh pr merge\n"
        "BODY\n"
        ')"'
    )
    assert _merge_segment(cmd) is None


def test_unbalanced_quote_fallback_is_deny_leaning_for_git():
    # The ValueError fallback in main() used `command.startswith("git")`, which
    # is fail-OPEN: an unterminated quote in a command not literally beginning
    # with `git` dropped the push gate entirely, while the merge gate on the
    # same input still fired via its regex-anywhere fallback. Both fallbacks
    # must lean the same way. Drives the real hook end to end - asserting the
    # predicate in isolation would pass even if main() reverted to startswith.
    out = _run_hook('echo "intro\ngit push origin main')
    assert out.get("permissionDecision") == "deny", out


# --- heredoc BODIES are raw text, not quoted shell ---------------------------
# Quote tracking must PAUSE inside a heredoc body: one apostrophe in prose
# otherwise opens a quote that swallows the terminator's newline, the heredoc
# never terminates, and the fail-closed re-judge hands shlex the same
# apostrophe - the false refusal above, re-entering through the heredoc door.


def test_apostrophe_in_heredoc_body_does_not_swallow_the_terminator():
    cmd = "cat <<EOF\nthis doesn't apply cleanly\nEOF\necho after"
    assert git_protection._command_segments(cmd) == [
        ["cat", "<<", "EOF"], ["echo", "after"]]


def test_apostrophe_in_heredoc_body_still_hides_no_real_command():
    # The body exemption is unchanged: with no EOF terminator the body is
    # re-judged, and the apostrophe then raises out of shlex into the
    # deny-leaning whole-command fallback. Either route must end in a deny.
    out = _run_hook("cat <<EOF\nit doesn't matter\ngit push origin main")
    assert out.get("permissionDecision") == "deny", out


# --- `$( ... )` bodies are COMMANDS, even inside double quotes ---------------
# A `$(` reached inside double quotes still runs its body, so the quote-aware
# split above kept `"$(<newline>gh pr merge 1)"` as one token and the gate saw
# no merge segment at all. Before the split it was caught only by accident, via
# the ValueError fallback. Bodies are now re-segmented recursively.


def test_multiline_substitution_hiding_a_merge_is_caught():
    assert _merge_segment('X="$(gh pr merge 1 --auto\n)"') is not None


def test_singleline_substitution_hiding_a_push_is_caught():
    assert _git_segments('echo "$(git push origin main)"')


def test_substitution_inside_single_quotes_is_not_executed():
    # No expansion in single quotes, so this really is inert prose.
    assert _git_segments('fno mail send x \'see "$(git push origin main)"\'') == []


if __name__ == "__main__":
    for _name, _fn in sorted(globals().items()):
        if _name.startswith("test_") and callable(_fn):
            _fn()
    print("ok: all git-protection scenarios pass")
