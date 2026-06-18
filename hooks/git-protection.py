#!/usr/bin/env python3
"""
Global Git Protection Hook

Blocks:
- Direct pushes to main/master/develop/dev (bypass phrase: "Push to Main")
- git commit/push with --no-verify (approve via touch approve_no_verify.flag)
- gh pr merge without two-factor state+artifact verification

Allowed without gate:
- gh pr create (ad-hoc development is legitimate; merge gate enforces
  pipeline discipline at the shipping boundary)

Two-factor merge verification:
  (1) target-state.md (NOT megawalk-state.md) with
      status: IN_PROGRESS and auto_merge_approved: true, AND
  (2) External review evidence: either external_review_passed: skipped in
      state (explicit --no-external) OR a matching artifact at
      <repo>/.fno/artifacts/external-<session_id>.md with
      phase: external and session_id matching the state file.

No single-use flag override for auto-merge: the LLM would have been able
to self-create it. The two-factor artifact check serves the same
human-authorization purpose with a much stronger audit trail.

Megawalk-state.md deliberately does NOT authorize gh pr merge. When
megawalk is invoked, only target (via its internal Phase 7a pipeline) can
merge a PR; the outer megawalk thread must not. PR creation, however, is
not a megawalk-only concern - it's always allowed.
"""
import json
import os
import re
import subprocess
import sys
import time
from datetime import datetime, timedelta
from pathlib import Path

# State file for tracking approvals
STATE_DIR = Path.home() / ".claude" / "state"
STATE_FILE = STATE_DIR / "git-protection.json"

# Protected branches - NO COWBOY CODING
PROTECTED_BRANCHES = ["main", "master", "develop", "dev"]

# ==========================================
# GIT PUSH PATTERNS
# ==========================================
GIT_PUSH_PATTERNS = [
    # Standard push
    r'git\s+push',
    # Push with flags
    r'git\s+push\s+(-[a-zA-Z]+\s+)*',
    # Push with --no-verify (ESPECIALLY THIS)
    r'git\s+push.*--no-verify',
    # Push upstream
    r'git\s+push\s+-u',
    r'git\s+push\s+--set-upstream',
]

# ==========================================
# BLOCKED: --no-verify PATTERNS
# ==========================================
NO_VERIFY_PATTERNS = [
    r'git\s+commit.*--no-verify',
    r'git\s+push.*--no-verify',
]

# ==========================================
# ALLOWED GIT COMMANDS
# ==========================================
ALLOWED_GIT_PATTERNS = [
    r'git\s+status',
    r'git\s+log',
    r'git\s+diff',
    r'git\s+branch',
    r'git\s+checkout',
    r'git\s+fetch',
    r'git\s+pull',
    r'git\s+add',
    r'git\s+commit(?!.*--no-verify)',  # Allow commit but NOT with --no-verify
    r'git\s+stash',
    r'git\s+show',
    r'git\s+config',
    r'git\s+remote',
    r'git\s+tag',
]

def load_state():
    """Load approval state."""
    try:
        with open(STATE_FILE, 'r') as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return {
            "bypass_phrase": "Push to Main",
            "last_approval": None,
            "approval_expires": None,
            "last_blocked_command": None,
        }

def save_state(state):
    """Save approval state."""
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    with open(STATE_FILE, 'w') as f:
        json.dump(state, f, indent=2)

def has_recent_approval(state):
    """Check if we have recent approval."""
    if not state.get("approval_expires"):
        return False

    try:
        expires = datetime.fromisoformat(state["approval_expires"])
        return datetime.now() < expires
    except Exception:
        return False

def check_for_bypass_phrase(state):
    """Check recent command history for bypass phrase."""
    bypass_phrase = state.get("bypass_phrase", "Push to Main")

    # Check environment variable (set by user-messages.py hook)
    recent_message = os.environ.get("CLAUDE_RECENT_USER_MESSAGE", "")

    if bypass_phrase.lower() in recent_message.lower():
        # Grant approval for 2 minutes
        state["last_approval"] = datetime.now().isoformat()
        state["approval_expires"] = (datetime.now() + timedelta(minutes=2)).isoformat()
        save_state(state)
        return True

    return False

def extract_branch_from_push(command):
    """Extract the target branch from a git push command."""
    # Pattern: git push [remote] [branch]
    # Examples:
    #   git push origin main
    #   git push main
    #   git push -u origin main
    #   git push --set-upstream origin main

    # Remove flags
    cleaned = re.sub(r'\s+(-[a-zA-Z]+|--[a-zA-Z-]+)', ' ', command)

    # Match: git push [optional remote] [branch-or-refspec]
    match = re.search(r'git\s+push\s+(?:\S+\s+)?(\S+)', cleaned)
    if match:
        refspec = match.group(1)
        # Handle refspecs like `feature:main` by extracting the destination
        # branch (right side of the colon). Without this, `git push origin
        # feature:main` bypasses the protected-branch check because
        # `feature:main` is not literally in PROTECTED_BRANCHES.
        return refspec.split(':')[-1] if ':' in refspec else refspec

    return None

def get_current_branch():
    """Try to get current branch from git (if in git repo)."""
    try:
        result = subprocess.run(
            ['git', 'symbolic-ref', '--short', 'HEAD'],
            capture_output=True,
            text=True,
            timeout=1
        )
        if result.returncode == 0:
            return result.stdout.strip()
    except Exception:
        pass
    return None

def is_push_to_protected_branch(command):
    """Check if command is pushing to a protected branch."""
    # Check if it's a push command
    is_push = False
    for pattern in GIT_PUSH_PATTERNS:
        if re.search(pattern, command, re.IGNORECASE):
            is_push = True
            break

    if not is_push:
        return False, None

    # Extract explicit branch from command
    explicit_branch = extract_branch_from_push(command)

    # Check explicit branch
    if explicit_branch and explicit_branch in PROTECTED_BRANCHES:
        return True, explicit_branch

    # If no explicit branch, check current branch
    current_branch = get_current_branch()
    if current_branch and current_branch in PROTECTED_BRANCHES:
        return True, current_branch

    # Check for patterns like "git push" with no args (pushes current branch)
    if re.match(r'git\s+push\s*$', command.strip()):
        if current_branch and current_branch in PROTECTED_BRANCHES:
            return True, current_branch

    return False, None

def is_using_no_verify(command):
    """Check if command uses --no-verify flag."""
    return bool(re.search(r'--no-verify', command))

def is_allowed_git_command(command):
    """Check if git command is in allowed list."""
    for pattern in ALLOWED_GIT_PATTERNS:
        if re.search(pattern, command, re.IGNORECASE):
            return True
    return False

def _candidate_repo_roots():
    """Return repo roots to search for an active target session, in priority
    order: the hook's own repo root first (fast path / backward compat), then
    every git worktree. Deduplicated, order preserved.

    /target frequently runs in a worktree while the Claude conversation cwd
    (and therefore this hook's cwd) is pinned to the canonical checkout. The
    canonical target-state.md is then a stale/unrelated session, so resolving
    only from cwd misses the real active session. Enumerating worktrees finds
    it. Silent fallback to just the cwd root if `git worktree list` is
    unavailable (older git, transient failure) - never worse than before.
    """
    roots = []

    def _add(p):
        try:
            rp = Path(p).resolve()
        except Exception:
            return
        if rp not in roots:
            roots.append(rp)

    try:
        result = subprocess.run(
            ['git', 'rev-parse', '--show-toplevel'],
            capture_output=True, text=True, timeout=1,
        )
        if result.returncode == 0 and result.stdout.strip():
            _add(result.stdout.strip())
    except Exception:
        pass

    try:
        result = subprocess.run(
            ['git', 'worktree', 'list', '--porcelain'],
            capture_output=True, text=True, timeout=2,
        )
        if result.returncode == 0:
            for line in result.stdout.splitlines():
                if line.startswith('worktree '):
                    _add(line[len('worktree '):].strip())
    except Exception:
        pass

    return roots


def _parse_active_state(state_file, freshness_limit=3600):
    """Return the frontmatter dict for an active target session at state_file,
    else None. Active means: file exists, mtime within freshness_limit, and
    frontmatter status is IN_PROGRESS.

    All filesystem access is guarded: discovery now spans every worktree, so a
    single inaccessible candidate (e.g. a permission-restricted mount) must be
    skipped, not allowed to raise and abort the whole merge-authorization
    check. Returning None here just means "this candidate isn't an active
    session"; scanning continues with the next."""
    try:
        if not state_file.exists():
            return None
        age = time.time() - state_file.stat().st_mtime
    except OSError as e:
        print(f"[git-protection] skip {state_file}: {e}", file=sys.stderr)
        return None
    if age > freshness_limit:
        return None
    try:
        text = state_file.read_text()
    except Exception as e:
        print(f"[git-protection] skip {state_file}: {e}", file=sys.stderr)
        return None
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        return None
    fm = {}
    for line in lines[1:]:
        s = line.strip()
        if s == "---":
            break
        if ":" in s:
            k, v = s.split(":", 1)
            fm[k.strip()] = v.strip().strip('"').strip("'")
    if fm.get("status", "").upper() != "IN_PROGRESS":
        return None
    return fm


def _artifact_pr(artifact):
    """Return the pr_number string recorded in the external artifact's
    frontmatter, or None if absent/unreadable. Used to bind an active session
    to the PR being merged."""
    try:
        text = artifact.read_text()
    except Exception:
        return None
    m = re.search(r'^pr_number:\s*["\']?(\d+)["\']?\s*$', text, re.MULTILINE)
    return m.group(1) if m else None


def _get_active_target_session(prefer_pr=None):
    """Return (state_file, frontmatter_dict, repo_root) for the active target
    session that authorizes this merge, else (None, None, None).

    Active means:
    - <repo_root>/.fno/target-state.md exists (NOT megawalk -
      megawalk is forbidden from creating/merging PRs per HARD-GATE)
    - state file mtime is within the last hour
    - frontmatter status is IN_PROGRESS

    Candidate repo roots are the hook's own root plus every git worktree (see
    _candidate_repo_roots), so a session running in a worktree is found even
    when the hook's cwd is the canonical checkout. The cwd root is checked
    first.

    Selection FAILS CLOSED on ambiguity - widening discovery across worktrees
    must never let one session's auto_merge_approved + artifact authorize an
    unrelated PR:

    - prefer_pr is None (PR not parseable from the command, e.g. URL/branch/
      no-arg form): authorize only when exactly ONE active session exists.
      More than one and we cannot map the merge to a session -> deny.
    - prefer_pr given: a session whose external artifact records that exact PR
      wins. Otherwise any session whose artifact records a DIFFERENT PR is
      excluded (typo / wrong-PR protection); only "neutral" sessions whose
      artifact records no PR (backward compat: /pr check's artifact omits
      pr_number) may authorize, and only when exactly one remains -> else deny.

    The megawalk-state.md file deliberately does NOT authorize gh pr create or
    gh pr merge. Megawalk orchestrates target subagents; if target fails, megawalk
    must halt, not take over PR operations itself.
    """
    matches = []
    for repo_root in _candidate_repo_roots():
        state_file = repo_root / ".fno" / "target-state.md"
        fm = _parse_active_state(state_file)
        if fm is not None:
            matches.append((state_file, fm, repo_root))

    if not matches:
        return None, None, None

    if prefer_pr is None:
        # No PR to disambiguate on. Safe only when there is exactly one active
        # session; with several, fail closed rather than guess.
        if len(matches) == 1:
            return matches[0]
        return None, None, None

    # prefer_pr given. Exact artifact match is the precise, safe answer.
    neutral = []  # active sessions whose artifact records no PR (compat)
    for state_file, fm, repo_root in matches:
        sid = fm.get("session_id", "").strip()
        artifact = (repo_root / ".fno" / "artifacts" / f"external-{sid}.md") if sid else None
        recorded = _artifact_pr(artifact) if artifact else None
        if recorded is not None and recorded == str(prefer_pr):
            return state_file, fm, repo_root
        if recorded is None:
            neutral.append((state_file, fm, repo_root))
        # recorded but != prefer_pr -> conflicting, excluded entirely.

    # No exact match: only a single neutral session may authorize. Zero or
    # multiple -> cannot bind the merge to one session -> deny.
    if len(neutral) == 1:
        return neutral[0]
    return None, None, None


def _parse_merge_pr(command):
    """Extract the PR number a `gh pr merge` invocation targets, or None.

    Handles a bare number (`gh pr merge 356`), a PR URL
    (`gh pr merge https://github.com/o/r/pull/356`), and flags preceding the
    argument (`gh pr merge --squash 356`). Returns None for the branch-name
    form and the no-argument (current-branch) form, where the PR can't be
    determined from the command text alone - the caller then fails closed when
    more than one active session exists.

    Conservative on value-taking flags (e.g. `--body-file x`): the first
    non-flag token that is neither a number nor a /pull/<n> URL is treated as
    unknown and yields None, which only ever errs toward denial.
    """
    tokens = command.split()
    start = None
    for i in range(len(tokens) - 2):
        if (tokens[i].lower() == "gh" and tokens[i + 1].lower() == "pr"
                and tokens[i + 2].lower() == "merge"):
            start = i + 3
            break
    if start is None:
        return None
    for tok in tokens[start:]:
        if tok.startswith("-"):
            continue
        if tok.isdigit():
            return tok
        m = re.search(r'/pull/(\d+)/?$', tok)
        if m:
            return m.group(1)
        return None  # branch name or flag value -> PR unknown
    return None


def _check_pr_merge_allowed(command=""):
    """Return a reason string if gh pr merge is authorized, else None.

    Two-factor authorization required:
    1. Active target session with auto_merge_approved: true
    2. External review evidence:
       a. State flag external_review_passed: skipped (explicit --no-external), OR
       b. External review artifact file exists at
          .fno/artifacts/external-{session_id}.md with matching frontmatter
          (phase: external, session_id matches state file).

    State-file-only attestation is NOT sufficient. The artifact is written by
    /pr check when it completes; its presence proves external review actually
    ran this session. The LLM can still write the artifact, but doing so is a
    clear auditable violation rather than a one-line `touch`.

    `command` is the gh invocation; its PR number (when present) disambiguates
    which worktree session authorizes when several are active.
    """
    pr_number = _parse_merge_pr(command)

    state_file, fm, repo_root = _get_active_target_session(prefer_pr=pr_number)
    if state_file is None:
        return None
    if fm.get("auto_merge_approved", "").lower() not in ("true", "yes", "1"):
        return None

    # Factor 2a: external review explicitly skipped via --no-external / config
    ext_passed = fm.get("external_review_passed", "").lower()
    if ext_passed == "skipped" or fm.get("no_external", "").lower() in ("true", "yes", "1"):
        return f"active {state_file.name} (external review skipped by flag)"

    # Factor 2b: external review artifact must exist with matching session_id
    session_id = fm.get("session_id", "").strip()
    if not session_id:
        return None  # no session_id -> cannot verify artifact binding

    artifact = repo_root / ".fno" / "artifacts" / f"external-{session_id}.md"
    if not artifact.exists():
        return None
    age = time.time() - artifact.stat().st_mtime
    if age > 3600:
        return None
    try:
        artifact_text = artifact.read_text()
    except Exception:
        return None
    # Artifact frontmatter must bind session_id and phase. Use anchored
    # regex rather than substring check: a substring like
    # `session_id: abc` would false-match an artifact with
    # `session_id: abcdef` or a commented reference. Exact line matches
    # (optional quoting tolerated) prevent that.
    sid_pattern = rf'^session_id:\s*["\']?{re.escape(session_id)}["\']?\s*$'
    phase_pattern = r'^phase:\s*["\']?external["\']?\s*$'
    if not re.search(sid_pattern, artifact_text, re.MULTILINE):
        return None
    if not re.search(phase_pattern, artifact_text, re.MULTILINE):
        return None

    return f"active {state_file.name} + external review artifact ({int(age)}s old)"


def main():
    """Main hook enforcement logic."""
    try:
        input_data = json.load(sys.stdin)
    except Exception:
        sys.exit(0)  # Allow if we can't parse input

    tool_name = input_data.get("tool_name", "")
    tool_input = input_data.get("tool_input", {})

    # Only check Bash commands
    if tool_name != "Bash":
        sys.exit(0)

    command = tool_input.get("command", "").strip()

    # ==========================================
    # gh pr create - always allowed (ad-hoc dev is legit; the merge gate
    # is where pipeline discipline is enforced)
    # ==========================================

    # ==========================================
    # gh pr merge - allow only with two-factor (state + artifact) verification
    # ==========================================
    if re.search(r'gh\s+pr\s+merge', command, re.IGNORECASE):
        allow_reason = _check_pr_merge_allowed(command)
        if allow_reason:
            output = {
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "allow",
                    "permissionDecisionReason": f"[fno auto-merge] {allow_reason}",
                }
            }
            print(json.dumps(output))
            sys.exit(0)

        reason = """╔════════════════════════════════════════════════════════════════╗
║  🚫 BLOCKED: gh pr merge (two-factor check failed)
╚════════════════════════════════════════════════════════════════╝

Command: """ + command + """

Auto-merge from Claude Code requires ALL of:
  1. Top-level `config.auto_merge.enabled: true` in settings.yaml
  2. Active target state file with `auto_merge_approved: true`
     (megawalk-state.md does NOT authorize merge - target owns shipping)
  3. Either:
     a. `external_review_passed: skipped` in state (explicit --no-external), OR
     b. External review artifact at
        <repo>/.fno/artifacts/external-<session_id>.md
        with matching frontmatter (phase: external, session_id: <sid>)

The artifact proves /pr check actually ran for this session. A stale
or missing artifact blocks the merge even if the state flag is true.

There is NO single-use override flag. If /pr check was skipped or failed,
the correct recovery is to run it again or explicitly configure
--no-external. Do not forge the artifact.

If you expected this to work:
  - Run /target L <plan> (runs /pr check by default and writes the artifact)
  - Or set `--no-external` / config.external_reviewer: none to skip
    intentionally; the state file will show external_review_passed: skipped
  - Check that .fno/artifacts/external-*.md exists and matches
    the session_id in state file frontmatter

═══════════════════════════════════════════════════════════════════
"""
        output = {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        }
        print(json.dumps(output))
        sys.exit(0)

    # ==========================================
    # ALLOW: Non-git commands
    # ==========================================
    if not command.startswith('git'):
        sys.exit(0)

    # ==========================================
    # CHECK: User approval for --no-verify
    # ==========================================
    approval_flag = STATE_DIR / "approve_no_verify.flag"
    has_approval = False

    if approval_flag.exists():
        mtime = approval_flag.stat().st_mtime
        age_seconds = time.time() - mtime

        if age_seconds < 300:  # 5 minutes
            has_approval = True
            # Don't remove flag yet - remove after successful use
        else:
            # Expired approval
            approval_flag.unlink()

    # ==========================================
    # BLOCK: --no-verify usage (unless approved)
    # ==========================================
    for pattern in NO_VERIFY_PATTERNS:
        if re.search(pattern, command, re.IGNORECASE):
            # Check if approved by user
            if has_approval:
                # Remove approval flag (one-time use)
                if approval_flag.exists():
                    approval_flag.unlink()
                # Allow the command
                output = {
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "permissionDecisionReason": "[Approved] User approved --no-verify commit",
                    }
                }
                print(json.dumps(output))
                sys.exit(0)
            reason = f"""╔════════════════════════════════════════════════════════════════╗
║  🚫 BLOCKED: --no-verify flag detected
╚════════════════════════════════════════════════════════════════╝

Command: {command}

⛔ AI assistants MUST NOT bypass git hooks with --no-verify ⛔

Why this is blocked:
  • Hooks exist to prevent mistakes (Lambda anti-patterns, security, etc.)
  • Bypassing hooks defeats their purpose
  • Only humans can make the judgment call to override hooks
  • This protects code quality and prevents production issues

The proper workflow:

  1. Review the hook's feedback/warnings
  2. Fix the issues the hook identified
  3. Commit normally (without --no-verify)

═══════════════════════════════════════════════════════════════════

⚠️  To approve this --no-verify commit:

Run this command:
  touch ~/.claude/state/approve_no_verify.flag

Then I'll retry the commit automatically.
Approval expires after 5 minutes and is single-use.

═══════════════════════════════════════════════════════════════════
"""
            output = {
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                }
            }
            print(json.dumps(output))
            sys.exit(0)

    # ==========================================
    # ALLOW: Safe git commands
    # ==========================================
    if is_allowed_git_command(command):
        sys.exit(0)

    # ==========================================
    # CHECK: Push to protected branch
    # ==========================================
    is_protected, branch = is_push_to_protected_branch(command)

    if is_protected:
        state = load_state()
        using_no_verify = is_using_no_verify(command)

        # Extra angry message for --no-verify
        no_verify_warning = ""
        if using_no_verify:
            no_verify_warning = """
⚠️  DETECTED: --no-verify flag
⚠️  This is EXACTLY the behavior we're trying to prevent!
⚠️  Bypassing git hooks is NOT acceptable for protected branches.
"""

        # Check for recent approval or bypass phrase
        if has_recent_approval(state) or check_for_bypass_phrase(state):
            print(f"[Git Protection: Approved] Emergency push to {branch}: {command}", file=sys.stderr)
            sys.exit(0)

        # BLOCK
        reason = f"""╔════════════════════════════════════════════════════════════════╗
║  🚫 BLOCKED: Direct push to protected branch '{branch}'
╚════════════════════════════════════════════════════════════════╝

Command: {command}
{no_verify_warning}
⛔ THIS IS NOT COWBOY CODING. ⛔

Protected branches: {', '.join(PROTECTED_BRANCHES)}

Why this is blocked:
  • Direct pushes to {branch} bypass code review
  • Changes should be reviewed by the team
  • This protects against accidental destructive changes
  • CI/CD expects PRs, not direct pushes

The proper workflow:

  1. Create a feature branch:
     git checkout -b feature/your-feature-name

  2. Make your changes and commit

  3. Push the feature branch:
     git push origin feature/your-feature-name

  4. Create a pull request for review

  5. Merge after approval

═══════════════════════════════════════════════════════════════════

⚠️  EMERGENCY OVERRIDE (use with extreme caution):

If you have a legitimate emergency (production down, critical hotfix):

1. User must explicitly say: "Push to Main" (or similar)
2. Approval expires after 2 minutes
3. Document the emergency push in PR after the fact

Bypass phrases: Push to Main, push to main, Emergency Push

═══════════════════════════════════════════════════════════════════
"""

        # Store blocked command for debugging
        state["last_blocked_command"] = command
        save_state(state)

        output = {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        }
        print(json.dumps(output))
        sys.exit(0)

    # Command is safe, allow it
    sys.exit(0)


if __name__ == "__main__":
    main()
