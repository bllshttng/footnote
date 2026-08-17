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

Merge-gate override: touch ${FNO_HOME:-~/.fno}/merge-gate.disabled
Checked ONLY after the two-factor path fails, applies ONLY to
`gh pr merge`, expires after 5 minutes, consumed on use, appended to
merge-gate-overrides.log. That audit trail is what the old refusal of a
single-use override was really asking for.

No marker here can open main. The override does NOT reach the
protected-branch gate or the --no-verify gate, which are one protection
with two doors: --no-verify skips .git/hooks/pre-push, and that hook IS
the main-branch guard. Each keeps its own scoped control (the "Push to
Main" bypass phrase and approve_no_verify.flag).

Megawalk-state.md deliberately does NOT authorize gh pr merge. When
megawalk is invoked, only target (via its internal Phase 7a pipeline) can
merge a PR; the outer megawalk thread must not. PR creation, however, is
not a megawalk-only concern - it's always allowed.
"""
import json
import os
import re
import shlex
import subprocess
import sys
import time
from datetime import datetime, timedelta
from pathlib import Path

# Footnote state lives under ${FNO_HOME:-~/.fno}, never under the harness
# state dir (placement rule, ab-f063 Wave 2). This hook is stdlib-only and runs under bare
# python3 on a fresh plugin install, so it resolves FNO_HOME directly rather
# than importing fno.paths (the same accepted limitation every
# ${FNO_HOME:-$HOME/.fno} shell hook has). A custom config.state_dir in
# settings.yaml is therefore not honored here.
FNO_HOME = Path(os.environ.get("FNO_HOME") or (Path.home() / ".fno"))
STATE_FILE = FNO_HOME / "git-protection.json"
APPROVAL_FLAG = FNO_HOME / "approve_no_verify.flag"
# Merge-gate override, `gh pr merge` ONLY. Deliberately NOT named
# git-protection.disabled: that file was an unconditional pre-gate exit(0), so
# one touch dropped main-branch protection for every session on every harness
# lane. The rename leaves any stale old marker inert - fail-safe, no migration.
MERGE_GATE_MARKER = FNO_HOME / "merge-gate.disabled"
OVERRIDE_LOG = FNO_HOME / "merge-gate-overrides.log"
# Both markers expire and are consumed: a forgotten sentinel must not linger.
MARKER_TTL_SECONDS = 300

# Substitution forms that run a command without being a separate segment. Any of
# them disqualifies an authorization: a command substitution IS a second
# command, whatever its delimiter, and a PreToolUse allow would cover it.
_SUBSTITUTION_FORMS = ("`", "$(", "<(", ">(")

# Output redirection, matched as WHOLE operator tokens. `any(">" in tok)` also
# matched quoted argument text, so `-m "a > b"` and a message containing a
# markdown code span were refused with a message about several actions. `<` is
# absent on purpose: input redirection cannot run a command, and `<<` is the
# recommended escape.
_REDIRECT_TOKENS = {">", ">>", ">|", ">&", "&>", "&>>"}

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
# Subcommands this hook does not gate, matched POSITIONALLY (token[1]) rather
# than by regex over the segment text - see is_allowed_git_command for why the
# regex form was a bypass in one direction and a false refusal in the other.
ALLOWED_GIT_SUBCOMMANDS = {
    "status", "log", "diff", "branch", "checkout", "fetch", "pull", "add",
    "commit", "stash", "show", "config", "remote", "tag",
}

def _default_state():
    return {
        "bypass_phrase": "Push to Main",
        "last_approval": None,
        "approval_expires": None,
        "last_blocked_command": None,
    }

def load_state():
    """Load approval state from the FNO_HOME path. Missing or corrupt -> defaults.

    No legacy read-fallback from the old harness state dir: the only durable
    field is bypass_phrase, which no code path ever customizes, and the approval
    timestamps are 2-minute TTL. Migration would preserve nothing real while
    forcing a harness-dir reference that defeats the re-home (US2). A user blocked
    pre-upgrade is re-blocked once with the new path printed; self-healing.
    """
    try:
        with open(STATE_FILE, 'r') as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError, OSError):
        # OSError covers an unreadable path / a directory at STATE_FILE: a
        # PreToolUse crash silently drops the gate, so degrade to defaults.
        return _default_state()

def save_state(state):
    """Save approval state. Never raises: this runs on the protected-push deny
    path, and an unguarded OSError (read-only FNO_HOME, a directory at
    git-protection.json, a full disk) would propagate out of main() and exit
    non-zero, which a PreToolUse hook treats as a non-blocking error - so the
    push to main would proceed. Recording the attempt is best-effort; refusing
    is not. load_state already degrades to defaults for the same reason."""
    try:
        FNO_HOME.mkdir(parents=True, exist_ok=True)
        with open(STATE_FILE, 'w') as f:
            json.dump(state, f, indent=2)
    except OSError:
        pass

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

    # Remove flags, including an attached =value (so `--force-with-lease=origin/x`
    # is dropped whole; without this the leftover `=origin/x` shifts the
    # positional parse and a `... =origin/x origin main` reads the destination as
    # the remote, letting a force-with-lease to a protected branch slip through).
    cleaned = re.sub(r'\s+(-[a-zA-Z]+|--[a-zA-Z-]+)(=\S+)?', ' ', command)

    # Match: git push [optional remote] [branch-or-refspec]
    # IGNORECASE: GIT_PUSH_PATTERNS already matches case-insensitively, so a
    # case-SENSITIVE parse here made `GIT push origin main` look like a push with
    # no named branch - which fell through to the current-branch check and was
    # allowed from a feature branch. `GIT` resolves on a case-insensitive
    # filesystem (macOS), so that was a live hole, and fixing it at the parse
    # covers both the tokenized and the unbalanced-quote fallback path.
    match = re.search(r'git\s+push\s+(?:\S+\s+)?(\S+)', cleaned, re.IGNORECASE)
    if match:
        refspec = match.group(1)
        # Handle refspecs like `feature:main` by extracting the destination
        # branch (right side of the colon). Without this, `git push origin
        # feature:main` bypasses the protected-branch check because
        # `feature:main` is not literally in PROTECTED_BRANCHES.
        dest = refspec.split(':')[-1] if ':' in refspec else refspec
        # Normalize the destination before the membership test. Only the `:`
        # form was handled, so a fully-qualified or force-prefixed destination
        # never compared equal to a protected branch: `git push origin +main`
        # (a FORCE push to main from any branch) and
        # `git push origin refs/heads/main` were both allowed through.
        dest = dest.lstrip('+')
        for prefix in ("refs/heads/", "heads/"):
            if dest.startswith(prefix):
                dest = dest[len(prefix):]
                break
        return dest

    return None

def push_names_explicit_dest(command):
    """True only when the push unambiguously names a concrete destination
    branch: the `git push <remote> <branch>` form (>=2 positional args) whose
    target is not HEAD/@ (which resolve to the current branch). A remote-only
    `git push origin`, a bare `git push`, or `git push origin HEAD` is ambiguous
    (the destination is really the current branch) and returns False so the
    caller falls through to the current-branch check."""
    # Collapse shell line-continuations so multiline pushes count correctly.
    cleaned = re.sub(r'\\\s*\n', ' ', command)
    # Drop flags including an attached =value, so `--force-with-lease=origin/x`
    # leaves no positional token to miscount.
    cleaned = re.sub(r'\s+(-[a-zA-Z]+|--[a-zA-Z-]+)(=\S+)?', ' ', cleaned)
    m = re.search(r'git\s+push\b(.*)$', cleaned, re.IGNORECASE)
    if not m:
        return False
    args = m.group(1).split()
    return len(args) >= 2 and args[-1] not in ('HEAD', '@')

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

    # `--all` / `--mirror` push every local ref, so they update a protected
    # branch without ever naming one. They named no branch, so the refspec parse
    # read the REMOTE as the destination and the push sailed through from any
    # feature branch.
    if re.search(r'\s--(all|mirror)\b', command):
        return True, "all refs (--all/--mirror)"

    # Extract explicit branch from command
    explicit_branch = extract_branch_from_push(command)

    # Check explicit branch
    if explicit_branch and explicit_branch in PROTECTED_BRANCHES:
        return True, explicit_branch

    # An explicit, non-protected DESTINATION branch is authoritative regardless
    # of the session cwd's branch; without this a `git push origin feature/x`
    # from a cwd on main (every background /target ship) is wrongly blocked.
    # Gated on push_names_explicit_dest so an ambiguous single-token push
    # (`git push origin`, remote-only) or a current-branch push (`git push
    # origin HEAD`) still falls through and is blocked on a protected branch.
    if explicit_branch and push_names_explicit_dest(command):
        return False, None

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
    return bool(re.search(r'--no-verify', command, re.IGNORECASE))

def is_allowed_git_command(command):
    """True when this segment is an ungated git verb.

    POSITIONAL, not regex-over-the-string. Segments arrive shlex-rejoined with
    QUOTES STRIPPED, so a regex allowlist read argument text as if it were the
    command: `git commit --no-verify -m "see git log"` matched the `git log`
    pattern and waved the --no-verify commit straight through, while dropping the
    allowlist entirely made `git commit -m "fix: block git push origin main"`
    deny as a push to main. Both directions are the same root cause, and looking
    at the verb's POSITION rather than its text fixes both: token[1] is the
    subcommand no matter what the message says.

    A --no-verify token anywhere disqualifies, so an allowlisted verb cannot
    carry it past the gate below (the old pattern's negative lookahead).
    """
    parts = command.split()
    if len(parts) < 2:
        return False
    if any(p.lower() == "--no-verify" for p in parts):
        return False
    return parts[1].lower() in ALLOWED_GIT_SUBCOMMANDS

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
        # basename, to match _find_merge_segments: `/usr/bin/gh pr merge 42`
        # reached the merge gate but parsed no PR number here, and a None
        # prefer_pr authorizes against whichever single session is active rather
        # than against PR 42 - silently dropping the wrong-PR protection.
        if (tokens[i].rsplit("/", 1)[-1].lower() == "gh"
                and tokens[i + 1].lower() == "pr"
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


def _targets_other_repo(command):
    """True when the gh invocation names a repository other than this checkout.

    Every probe here reads THIS checkout, so a command pointed elsewhere is
    unanswerable and fails open. Four forms carry the override: `-R`,
    `--repo`, a `GH_REPO=` assignment, and a PR URL, which names its own repo
    and which the flag test cannot see. Prefix match, not equality: gh accepts
    the attached shorthand `-Rowner/repo` as readily as `-R owner/repo`.

    Quote-aware and single-token: a flag VALUE carrying `-R` or a PR link
    (`-t "-R fixes the deref"`, a subject citing another PR's URL) is one
    quoted argument, not an override, and must not disarm the vetoes. shlex
    keeps it one token; the whitespace and leading-dash rejects then drop it.
    A quoted value whose flag is absent still fails in gh itself, so nothing
    real is lost by refusing to match it here.
    """
    try:
        tokens = shlex.split(command)
    except ValueError:  # unbalanced quotes -> plain split, same as before
        tokens = command.split()
    for t in tokens:
        if " " in t:
            continue
        if t.startswith(("-R", "--repo", "GH_REPO=")):
            return True
        if "/pull/" in t and not t.startswith("-"):
            return True
    return False


def _stacked_base_refusal(command=""):
    """Refusal text when the PR's base no longer leads to the default branch.

    A PR merged into a base that already landed reports MERGED and ships
    nothing. `fno pr merge`, `fno pr verify` and the Rust auto-merge arm all
    call the same predicate; this hook is the fourth caller, and it is the only
    one that sees a BARE `gh pr merge`. Both harness wirings route through here
    (`hooks/hooks.json`, `hooks/codex-hooks.json`), which is most of the merge
    population in this repo - agents running gh through a tool call. A human
    typing gh in a plain terminal still bypasses it; only a required
    `stacked-base-guard` status context closes that.

    Shells to the CLI rather than importing it: this hook is stdlib-only and
    runs under bare python3 on a fresh plugin install, the same constraint that
    keeps it from importing `fno.paths`.

    Fails OPEN on everything except a confirmed refusal (exit 3). A missing
    `fno`, a timeout, an unevaluated probe (exit 4) - none of them may block a
    merge, because a guard whose own machinery is down must not become an
    outage. Exit 0 also covers the operator's documented bypass.

    The timeout sits well under the harness hook budget (60s by default, and
    neither hooks.json sets one) ON PURPOSE. This veto runs BEFORE the
    pre-existing two-factor merge gate, so a probe allowed to eat the whole
    budget would get the HOOK killed, and then no verdict is emitted at all -
    an unauthorized merge that the two-factor path would have denied sails
    through on a slow network. Fail open on this check, never on the hook.
    """
    pr_number = _parse_merge_pr(command)
    if not pr_number:
        return None  # branch-name or current-branch form: nothing to check
    # A merge aimed at another repository is unanswerable here: the lineage
    # check reads THIS checkout, so judging `gh pr merge 42 --repo other/repo`
    # against PR 42 here could deny a merge on a verdict about an unrelated PR.
    # Fail open, like every other unanswerable case in this function.
    if _targets_other_repo(command):
        return None
    return _fno_veto_refusal(
        ["pr", "base-lineage-check", pr_number],
        timeout=25,
        fallback=f"PR {pr_number}: base no longer leads to the default branch",
    )


def _fno_veto_refusal(args, timeout, fallback):
    """One fno-verb probe: deny on the verb's exit-3 refusal line, else open.

    The shared scaffold of both merge vetoes (lineage, coverage): fail OPEN on
    every way the probe itself can die - a missing `fno`, a timeout, any exit
    other than the verb's refusal code - because a guard whose own machinery is
    down must not become a merge outage. That includes an unknown-command exit
    from a `fno` deployment older than the verb (the rollout window): it is
    indistinguishable from any other usage error and fails open silently, which
    is why each veto's timeout stays well under the harness hook budget.
    """
    try:
        proc = subprocess.run(
            ["fno", *args],
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except Exception:  # noqa: BLE001 - incl. FileNotFoundError / TimeoutExpired
        return None
    if proc.returncode != 3:
        return None
    detail = (proc.stderr or proc.stdout or "").strip().splitlines()
    return detail[0] if detail else fallback


def _coverage_refusal(command=""):
    """Refusal text when the merge guard's coverage predicate says no.

    The hook's own two-factor check asks a different question than
    `fno pr merge` does: a manifest flag and a session-scoped external-review
    artifact, neither of which reads review coverage at the PR's head. Passing
    it was never evidence that the sanctioned merge primitive would agree. This
    consults the guard's predicate itself rather than carrying a third copy of
    it, so both paths refuse for the same reason and print the same sentence.

    Absence is a refusal here, not a shrug. `fno pr merge` refuses a missing
    or head-mismatched row, so a veto that waved those through would recreate
    the divergence it exists to close, on the precise input where nothing has
    reviewed the PR. A wrong deny costs one command: `fno pr merge` is the
    sanctioned primitive, this hook does not gate it, and it recomputes. A
    wrong allow lands an unreviewed merge. Deny is the cheap direction.

    Runs WITHOUT the recompute. `fno pr merge` fires the Rust producer once
    when no row describes the head, and that subprocess is budgeted in
    minutes. This veto runs inside a PreToolUse hook whose harness budget is
    60s, and a hook that gets killed emits no verdict at all, so a probe
    allowed to eat the budget would let an unauthorized merge through - the
    same reasoning the lineage veto's timeout carries. The cases only a
    recompute can answer are refused here and answered by `fno pr merge`,
    which is where the time is affordable.

    Fails OPEN only on a named instrument failure: exit 4, a missing `fno`, a
    timeout, another repository - and on an unknown-command exit from a `fno`
    deployment older than this verb (the rollout window), which the probe
    cannot distinguish from any other usage error. A guard whose own machinery
    is down must not become a merge outage. An empty read is not a machinery
    failure - it is the answer that nothing attested this head, and it exits 3.
    """
    pr_number = _parse_merge_pr(command)
    if not pr_number or _targets_other_repo(command):
        return None
    return _fno_veto_refusal(
        ["pr", "coverage-check", pr_number],
        timeout=15,
        fallback=f"PR {pr_number}: review coverage refused",
    )


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

    # A merge aimed at another repository cannot be authorized by THIS
    # checkout's session artifacts: the state file and the external-review
    # artifact below both belong to this repo, so an allow here would vouch
    # for an unrelated PR that happens to carry the same number. Same
    # predicate the two vetoes use; decline to authorize (the deny paths
    # still run), never answer a question about the wrong PR.
    if _targets_other_repo(command):
        return None

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


# ==========================================
# COMMAND-POSITION TOKENIZATION
# ==========================================
# Gate keywords (`git`, `gh pr merge`) must only fire when they sit at COMMAND
# position - the start of the string or right after a shell separator. A keyword
# buried inside a quoted argument (e.g. a `--details "... gh pr merge ..."`
# string) must NOT trip the gate (observed live 2026-07-06). The same tokenizer
# also closes the opposite hole: `cd /tmp && git push origin main` used to
# bypass the protected-branch gate because the string did not start with 'git'.

# Shell separators that end a command-position segment (within one physical
# line), and keywords after which a new command begins. Newlines are handled by
# splitting on physical lines BEFORE shlex: shlex in whitespace_split mode
# silently eats a newline as whitespace, so it can never be a separator token -
# a `git status\ngh pr merge` two-liner would otherwise collapse into one
# segment and hide the merge on line 2.
# `|&` is one token under punctuation_chars=True, so omitting it collapsed
# `true |& git push origin main` into a single segment whose argv[0] was `true` -
# the push gate never saw it. `;;` ends a case arm the same way `;` ends a
# command.
# `)` terminates a case arm pattern, so without it `case x in *) git push origin
# main;; esac` kept the arm body in the same segment as the case word and both
# gates saw nothing. Harmless for `(subshell)`: _effective_argv already strips a
# leading `(`.
_SEGMENT_SEPARATORS = {";", ";;", "&&", "||", "|", "|&", "&", ")"}
# Keywords and prefixes that occupy command position, so the REAL command is the
# next token. With only {then, do} here, every other shell construct hid a
# command from both gates and the hook emitted no decision at all:
# `! git push origin main`, `if git push origin main; then true; fi`,
# `{ git push origin main; }`, `while/until git push origin main; do ...`.
# Each is one token away from a bare push to main.
_SEGMENT_KEYWORDS = {"then", "do", "if", "elif", "else", "while", "until",
                     "!", "{", "}", "fi", "done", "esac", "case", "in",
                     "select", "for"}

# Command wrappers and env-assignment prefixes that precede the real executable.
# Stripped before identifying a segment's command so `sudo gh pr merge`,
# `env FOO=bar gh pr merge`, `GH_TOKEN=x gh pr merge`, `/usr/bin/gh pr merge`,
# and `(gh pr merge)` are all still recognized (the old regex-anywhere matcher
# caught them; command-position matching must not silently drop them).
_CMD_WRAPPERS = {"sudo", "env", "command", "time", "nice", "builtin", "exec",
                 "xargs", "nohup", "stdbuf", "eval", "coproc", "timeout",
                 "gtimeout", "setsid", "doas"}
# Shells that take the real command as the argument of `-c`.
_SHELL_RUNNERS = {"sh", "bash", "zsh", "dash", "ksh"}
# A wrapper option's value that is not dash-prefixed: `timeout 10`, `timeout 5m`.
_DURATION_RE = re.compile(r'^\d+(\.\d+)?[smhd]?$')
# Wrapper options whose value is a separate token, so the value is not the verb:
# `sudo -u x git ...`, `nice -n 5 git ...`.
_WRAPPER_VALUE_OPTS = {"-u", "-g", "-n", "-C", "-s", "-k", "-p", "--user",
                       "--group", "--chdir", "--signal", "--kill-after"}


_SHELL_RUNNER_RE = re.compile(r'\b(sh|bash|zsh|dash|ksh)\b[^|;&]*\s-\w*c\b')


def _is_dash_c(tok):
    """True for `-c` and the clustered short forms (`-lc`, `-cx`, `-ic`)."""
    return (tok.startswith("-") and not tok.startswith("--")
            and "c" in tok[1:])
_ASSIGN_RE = re.compile(r'^[A-Za-z_][A-Za-z0-9_]*=')


def _find_heredoc_opener(line):
    """Return (delimiter, is_dash) for the first heredoc opener on `line` that
    lies OUTSIDE a quoted region, else None.

    A `<<DELIM` inside single/double quotes is data, not an opener, so `echo
    "x <<EOF"` does not start a heredoc. A `<<DELIM` after an unquoted `#` is a
    comment, not an opener, so `echo ok # <<EOF` does not start one either
    (without this, the following line would be hidden as body content and a real
    command there would bypass the gate). Backslash and quote escaping are
    honored while scanning. Ambiguous shapes (`<<<` here-string, `<<` with no
    delimiter-like word after) yield None, so the line is segmented normally
    (the deny-leaning default) rather than granted a body exemption.
    """
    i, n = 0, len(line)
    quote = None
    while i < n:
        ch = line[i]
        if quote:
            # Inside double quotes a backslash escapes the next char; inside
            # single quotes nothing escapes.
            if ch == "\\" and quote == '"' and i + 1 < n:
                i += 2
                continue
            if ch == quote:
                quote = None
            i += 1
            continue
        if ch in ("'", '"'):
            quote = ch
            i += 1
            continue
        if ch == "\\" and i + 1 < n:
            i += 2  # escaped char outside quotes: skip it
            continue
        # An unquoted `#` at a word boundary starts a shell comment: stop
        # scanning, so a commented-out opener cannot start a hiding heredoc.
        # Over-matching is safe - forgoing a real (non-comment) opener only
        # forgets the body exemption, the deny-leaning direction.
        if ch == "#" and (i == 0 or not (line[i - 1].isalnum() or line[i - 1] == "_")):
            break
        if ch == "<" and i + 1 < n and line[i + 1] == "<":
            opener = _parse_heredoc_at(line, i + 2)
            if opener is not None:
                return opener
            i += 2  # `<<` but not an opener (e.g. `<<<`); keep scanning
            continue
        i += 1
    return None


def _parse_heredoc_at(line, j):
    """Given `line` and index `j` just past `<<`, parse an optional `-`, an
    optional quote, and the delimiter word. Return (delim, is_dash), or None if
    no valid delimiter follows (so the `<<` was something else). The closing
    quote, if any, is irrelevant: only the delimiter word is hunted as the
    terminator."""
    n = len(line)
    dash = False
    if j < n and line[j] == "-":
        dash = True
        j += 1
    if j < n and line[j] in ("'", '"'):
        j += 1  # opening quote on the delimiter; delimiter word follows
    m = re.match(r"[A-Za-z_][A-Za-z0-9_-]*", line[j:])
    if m is None:
        return None
    return m.group(0), dash


def _split_lines_outside_quotes(command):
    """Split `command` on newlines that lie OUTSIDE any quoted region.

    A newline inside an open quote is part of ONE argument, not a command
    boundary: `fno mail send x "...\\n...\\n..."`, a `-m` commit body, a
    `--details` string. Splitting there hands the next fragment to shlex with an
    unbalanced quote, which raises, and the caller then falls back to matching
    the guarded phrase ANYWHERE in the whole command - so any message whose BODY
    quoted a guarded invocation was refused, including a worker's review report
    about merge behaviour (observed live 2026-08-06).

    A newline OUTSIDE quotes still splits, so `git status\\ngh pr merge 1` is
    still judged as two command positions. An unterminated quote never closes,
    so the rest stays one line and shlex still raises - the deny-leaning
    direction, unchanged.

    Quoting rules do NOT apply inside a heredoc BODY: the shell reads raw lines
    until the terminator, so one apostrophe in prose (`this doesn't apply`) must
    not open a quote that swallows the terminator's newline. Without this the
    heredoc never terminates, the fail-closed path re-judges the body, and shlex
    raises on that same apostrophe - the exact false refusal this function
    exists to remove, re-entering through the heredoc door.
    """
    lines, buf = [], []
    quote = None
    hd_delim, hd_dash = None, False
    i, n = 0, len(command)
    while i < n:
        ch = command[i]
        if hd_delim is not None:
            if ch == "\n":
                line = "".join(buf)
                lines.append(line)
                buf = []
                term = line.lstrip("\t") if hd_dash else line
                if term == hd_delim:
                    hd_delim, hd_dash = None, False
            else:
                buf.append(ch)
            i += 1
            continue
        if quote:
            # Inside double quotes a backslash escapes the next char; inside
            # single quotes nothing escapes (same rule as _find_heredoc_opener).
            if ch == "\\" and quote == '"' and i + 1 < n:
                buf.append(command[i:i + 2])
                i += 2
                continue
            if ch == quote:
                quote = None
            buf.append(ch)
            i += 1
            continue
        if ch in ("'", '"'):
            quote = ch
            buf.append(ch)
            i += 1
            continue
        if ch == "\\" and i + 1 < n:
            buf.append(command[i:i + 2])  # escaped char outside quotes
            i += 2
            continue
        if ch == "\n":
            line = "".join(buf)
            lines.append(line)
            buf = []
            opener = _find_heredoc_opener(line)
            if opener is not None:
                hd_delim, hd_dash = opener
            i += 1
            continue
        buf.append(ch)
        i += 1
    lines.append("".join(buf))
    return lines


def _substitution_bodies(line):
    """Return the body of each `$( ... )` the shell will EXECUTE on `line`.

    A `$(` reached inside DOUBLE quotes still runs its body as commands, so the
    quote-aware line split above keeps `"$(<newline>gh pr merge 1)"` as one
    token and the gate never sees a merge segment. The bodies are re-segmented
    (recursively) so a real invocation cannot hide behind an interpolation.

    Single-quoted regions are skipped - no expansion happens there. Backticks
    are deliberately NOT treated as substitution: markdown code spans in PR and
    mail prose use them constantly, and that prose is what the quoting rules
    here exist to protect.
    """
    out = []
    i, n, quote = 0, len(line), None
    while i < n:
        ch = line[i]
        if quote == "'":
            quote = None if ch == "'" else quote
            i += 1
            continue
        if quote == '"' and ch == "\\" and i + 1 < n:
            i += 2
            continue
        if ch == "$" and i + 1 < n and line[i + 1] == "(":
            depth, j = 1, i + 2
            while j < n and depth:
                depth += (line[j] == "(") - (line[j] == ")")
                j += 1
            if depth == 0:  # unbalanced `$(` falls through to normal scanning
                out.append(line[i + 2:j - 1])
                i = j
                continue
        if quote is None and ch in ("'", '"'):
            quote = ch
        elif ch == quote:
            quote = None
        i += 1
    return out


def _command_segments(command, _depth=0):
    """Split a shell command into command-position segments (lists of tokens).

    Collapses backslash line-continuations, then walks physical lines with
    HEREDOC awareness, then segments each command line at shell separators.
    Each segment is a token run that begins a command. Uses stdlib shlex in
    POSIX mode so quoted arguments stay single tokens and separators inside
    quotes are not treated as separators. Raises ValueError on unbalanced
    quotes in a command line; the caller then falls back to legacy whole-command
    matching.

    Heredoc bodies are CONTENT, not command positions (Defect B): a line inside
    a `<<DELIM ... DELIM` body that merely quotes a guarded git invocation must
    not be judged as one - this is what fired whenever a command carried prose
    about git (documenting, testing, or filing a bug about these guards). An
    opener with no matching terminator fails CLOSED: the body is re-judged as
    commands (AC8-EDGE) so an unterminated body can never smuggle a real
    invocation past the gate.
    """
    # A backslash immediately before a newline is a shell line-continuation:
    # the shell joins the two physical lines into one logical command. Collapse
    # them FIRST, else `git push \<newline>origin main` or
    # `git commit \<newline>--no-verify` would split across physical lines and be
    # judged with the branch target / flag in isolation - a gate bypass (gemini
    # review, PR #227). Removed (not spaced) to match shell semantics, so a
    # mid-token continuation like `git pu\<newline>sh` rejoins to `git push`.
    command = re.sub(r'\\\r?\n', '', command)
    lines = _split_lines_outside_quotes(command)
    segments = []
    heredoc_delim = None  # terminator to seek; None when not inside a body
    heredoc_dash = False
    for line in lines:
        if heredoc_delim is not None:
            # Body line. For <<- the shell strips leading TABS from the
            # terminator (not spaces); match that so the exit test is faithful.
            term = line.lstrip("\t") if heredoc_dash else line
            if term == heredoc_delim:
                heredoc_delim = None
                heredoc_dash = False
            continue  # body is content either way; never a command position
        if line.strip():
            segments.extend(_segments_one_line(line))
            if _depth < 3:  # ponytail: 3 is plenty; deeper nesting is not real
                for body in _substitution_bodies(line):
                    segments.extend(_command_segments(body, _depth + 1))
        opener = _find_heredoc_opener(line)
        if opener is not None:
            # Body begins on the NEXT physical line; this opener line has
            # already been segmented above (its command prefix is judged).
            heredoc_delim, heredoc_dash = opener
    if heredoc_delim is not None:
        # Unterminated heredoc: fail CLOSED (AC8-EDGE). Drop the body exemption
        # and re-judge every line as a command position, so a guarded
        # invocation in an unterminated body is still caught rather than hidden.
        segments = []
        for line in lines:
            if line.strip():
                segments.extend(_segments_one_line(line))
    return segments


def _segments_one_line(line):
    lexer = shlex.shlex(line, punctuation_chars=True, posix=True)
    lexer.whitespace_split = True
    tokens = list(lexer)  # ValueError on unbalanced quotes
    segments, current = [], []
    at_command_start = True
    for tok in tokens:
        if tok in _SEGMENT_SEPARATORS:
            if current:
                segments.append(current)
                current = []
            at_command_start = True
            continue
        if at_command_start and tok in _SEGMENT_KEYWORDS:
            # keyword occupies command position; the real command is next token
            continue
        current.append(tok)
        at_command_start = False
    if current:
        segments.append(current)
    return segments


def _effective_argv(seg):
    """Strip a leading run of subshell `(`, env-assignments (NAME=value), and
    command wrappers (sudo/env/...) so the real executable token lands at
    argv[0]. Keeps a wrapper prefix from hiding a gated verb."""
    i, n = 0, len(seg)
    saw_wrapper = False
    while i < n:
        tok = seg[i]
        if tok == "(" or _ASSIGN_RE.match(tok):
            i += 1
            continue
        # Lowercased for the same reason the segment finders are: on a
        # case-insensitive filesystem `ENV git push origin main` and
        # `SUDO git push ...` resolve and run, and a case-sensitive wrapper test
        # left the real verb buried at argv[1] so no gate ever saw it.
        if tok.rsplit("/", 1)[-1].lower() in _CMD_WRAPPERS:
            i += 1
            saw_wrapper = True
            continue
        # `sh -c "..."`, and the clustered forms people actually type: `bash -lc`,
        # `sh -cx`, `bash -ic`. Matching only the exact token `-c` covered the
        # one spelling nobody uses.
        if tok.rsplit("/", 1)[-1].lower() in _SHELL_RUNNERS:
            # Scan past the runner's OWN options for its -c: testing only the
            # very next token missed `zsh -f -c ...` and `bash -l -c ...`.
            j = i + 1
            while j < n and seg[j].startswith("-") and not _is_dash_c(seg[j]):
                j += 1
            if j < n and _is_dash_c(seg[j]) and j + 1 < n:
                i = j + 1
                continue
        # A wrapper's OWN options, and their values: `env -i`, `nice -n 5`,
        # `timeout 10`, `sudo -u x`. Stopping at the first non-wrapper token let
        # any optioned wrapper hide the verb entirely.
        if saw_wrapper and tok in _WRAPPER_VALUE_OPTS and i + 1 < n:
            i += 2          # option plus its value: `sudo -u x`, `nice -n 5`
            continue
        if saw_wrapper and (tok.startswith("-") or _DURATION_RE.match(tok)):
            i += 1
            continue
        break
    argv = seg[i:]
    # A wrapper's argument is one quoted token holding a whole command, so the
    # verb is inside it rather than at argv[0]: `eval "git push origin main"`
    # and `bash -c "git push origin main"` were invisible while the UNQUOTED
    # `eval git push ...` was caught - protection that only covered the form
    # nobody writes. Re-tokenize so the real verb surfaces.
    if len(argv) == 1 and any(c.isspace() for c in argv[0]):
        try:
            inner = shlex.split(argv[0])
        except ValueError:
            inner = argv[0].split()
        if inner:
            return _effective_argv(inner)
    return argv


# Git's global options, which sit BEFORE the subcommand. The value-taking ones
# swallow the next token when written separately (`-C /repo`, `-c a=b`).
_GIT_GLOBAL_VALUE_OPTS = {"-C", "-c", "--git-dir", "--work-tree", "--namespace",
                          "--exec-path", "--super-prefix"}
_GIT_GLOBAL_FLAGS = {"-p", "--paginate", "--no-pager", "--bare", "--no-replace-objects",
                     "--literal-pathspecs", "--glob-pathspecs", "--noglob-pathspecs",
                     "--icase-pathspecs", "--no-optional-locks"}


def _strip_git_global_opts(argv):
    """Drop git's global options so argv[1] is the real subcommand.

    Returns argv[0] followed by the subcommand onward. A hooksPath override is
    detected separately by _sets_hooks_path, from the ORIGINAL argv - carrying
    its value through in the returned string made the push parsers read
    `core.hooksPath=x` as a named destination branch.
    """
    head, rest = argv[:1], argv[1:]
    i, n = 0, len(rest)
    while i < n:
        tok = rest[i]
        if tok in _GIT_GLOBAL_VALUE_OPTS:
            i += 2          # option plus its value
            continue
        if tok in _GIT_GLOBAL_FLAGS or (
                tok.startswith("--") and "=" in tok
                and tok.split("=", 1)[0] in _GIT_GLOBAL_VALUE_OPTS):
            i += 1
            continue
        break
    return head + rest[i:]


def _sets_hooks_path(argv):
    """True when this git invocation points core.hooksPath elsewhere, which
    disables .git/hooks/pre-push - and that hook IS the protected-branch guard,
    so it is a --no-verify by another name.

    POSITIONAL, over argv: a substring scan of the rejoined segment refused
    `git commit -m "docs: explain core.hooksPath guard"`, re-creating the exact
    quote-stripped false refusal this file fixed for the allowlist. Covers both
    spellings - the per-command `-c core.hooksPath=x` and the PERSISTENT
    `git config core.hooksPath x`, after which every later commit and push in
    the repo is unguarded with no flag on them at all.
    """
    for idx, tok in enumerate(argv):
        low = tok.lower()
        if low.startswith("core.hookspath="):
            return True                      # -c core.hooksPath=x
        if low == "core.hookspath":
            prev = argv[idx - 1].lower() if idx else ""
            if prev == "-c" or "config" in [a.lower() for a in argv[1:idx]]:
                return True                  # git config core.hooksPath x
    return False


def _writes_a_file(tokens):
    """True when a redirection would WRITE somewhere, as whole operator tokens.

    `2>&1` and `>&2` duplicate a file descriptor and write no file, so they are
    not disqualifying - refusing them denied an ordinary
    `git commit --no-verify -m ok 2>&1`. An `>&` followed by anything that is not
    a bare fd number is a write.
    """
    for idx, tok in enumerate(tokens):
        if tok not in _REDIRECT_TOKENS:
            continue
        if tok in (">&", "&>"):
            nxt = tokens[idx + 1] if idx + 1 < len(tokens) else ""
            if nxt.isdigit() or nxt.rstrip("-").isdigit():
                continue        # fd duplication, not a file write
        return True
    return False


def _has_unquoted_heredoc(command):
    """True when a REAL heredoc opener uses an UNQUOTED delimiter, whose body the
    shell expands (so a substitution can hide there). Quoted (<<'EOF') is
    literal, which is what makes the recommended `-F - <<'EOF'` escape work.

    Uses the quote-aware opener scan rather than a raw regex over the command:
    the raw form matched `<<` inside an ARGUMENT, so a commit message reading
    "shift << 2" was refused - the same raw-scan mistake _REDIRECT_TOKENS exists
    to avoid for `>`.

    Body lines are SKIPPED, exactly as _command_segments skips them: a body is
    data, so a message reading "fix <<EOF parsing" is not an opener. Without
    that, the recommended escape broke whenever the commit message happened to
    mention a heredoc.
    """
    lines = _split_lines_outside_quotes(command)
    i, n = 0, len(lines)
    while i < n:
        opener = _find_heredoc_opener(lines[i])
        if opener is None:
            i += 1
            continue
        delim, is_dash = opener
        # Derived from THIS opener's own delimiter, not a fresh scan of the whole
        # line: a line carrying a quoted `<<'X'` inside an argument plus a real
        # `<<EOF` later read as quoted and skipped the disqualifier.
        unquoted = bool(re.search(r"<<-?[ \t]*" + re.escape(delim) + r"(\s|$)",
                                  lines[i]))
        # Skip this heredoc's body: it is data, not command text.
        i += 1
        while i < n:
            term = lines[i].lstrip("\t") if is_dash else lines[i]
            i += 1
            if term == delim:
                break
        if unquoted:
            return True
    return False


def _find_merge_segments(segments):
    """Every `gh pr merge` segment, not just the first. One override consume
    must not authorize `gh pr merge 1 && gh pr merge 2 && gh pr merge 3`, which
    an only-the-first search allowed while logging one of the three."""
    out = []
    for seg in segments:
        argv = _effective_argv(seg)
        if (len(argv) >= 3 and argv[0].rsplit("/", 1)[-1].lower() == "gh"
                and argv[1].lower() == "pr" and argv[2].lower() == "merge"):
            out.append(" ".join(seg))
    return out


def _find_merge_segment(segments):
    """First `gh pr merge` segment, else None. TEST-FACING convenience only:
    main() needs the full list to count them, so it calls the plural directly.
    Kept so the segment-matching tests can assert one match readably."""
    found = _find_merge_segments(segments)
    return found[0] if found else None


def _find_git_segments(segments):
    """Return the executable-onward string of every segment whose command is
    `git` (wrapper/assignment prefix stripped). Catches compound commands a bare
    startswith('git') would miss, and git behind sudo/env/an assignment.

    Case-insensitive, to match _find_merge_segments: on a case-insensitive
    filesystem (macOS) `GIT push origin main` resolves and ran, while the
    lowercase-only comparison here handed the gate nothing at all.

    Git's own global options are dropped before the subcommand, so every
    downstream matcher sees a canonical `git <subcommand> ...`. Both gates key
    on the subcommand - one by `git\\s+push` adjacency, the other by token
    position - so `git -C /repo push origin main` and
    `git -c core.hooksPath=/dev/null push origin main` were invisible to both.
    """
    out = []
    for seg in segments:
        argv = _effective_argv(seg)
        if argv and argv[0].rsplit("/", 1)[-1].lower() == "git":
            normalized = _strip_git_global_opts(argv)
            # Encoded as a --no-verify flag rather than carried as a value: it
            # IS a --no-verify by another name, and a `--`-prefixed token is
            # stripped by the push parsers instead of read as a branch.
            if _sets_hooks_path(argv):
                normalized = normalized + ["--no-verify"]
            out.append(" ".join(normalized))
    return out


def _emit(decision, reason):
    """Print a PreToolUse permission decision as JSON."""
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    }))


def _fresh_marker(path, ttl_seconds=MARKER_TTL_SECONDS):
    """True if `path` exists and is younger than ttl_seconds. An expired marker
    is removed, so a forgotten sentinel cannot silently hold a gate open.
    Guarded so a filesystem hiccup never crashes the gate."""
    try:
        if path.exists():
            if time.time() - path.stat().st_mtime < ttl_seconds:
                return True
            path.unlink(missing_ok=True)  # expired
    except OSError:
        pass
    return False


def _claim_marker(path):
    """Atomically claim `path`; True only for the caller that removed it.

    unlink() IS the claim, not a cleanup afterwards. Two concurrent hook
    processes both pass a plain exists()/stat check, so `missing_ok=True` would
    let both authorize a merge from one operator approval - the loser here gets
    ENOENT instead. Guarded because an unguarded raise out of a PreToolUse hook
    fails OPEN on the very gate it was protecting."""
    try:
        os.unlink(path)
        return True
    except OSError:
        return False


def _audit(message):
    """Append-only record of a consumed merge-gate override; the log file is the
    durable trail (a PreToolUse hook's stderr is debug output, not user-facing -
    permissionDecisionReason is what the operator reads). One line per entry:
    `message` carries a command, so its newlines are flattened - a trail an
    embedded newline can forge entries into is not a trail. Records cwd as the
    caller, since the marker is global while the merge it authorizes belongs to
    one worktree.

    Returns True only if the trail was actually recorded. The caller REFUSES on
    False rather than allowing unrecorded: the trail is the whole justification
    for reintroducing an override, and an unwritable log (a directory at the log
    path, a read-only FNO_HOME) is the one case an agent could arrange. Never
    raises - an unguarded raise out of a PreToolUse hook fails open."""
    flat = " ".join(message.split())
    try:
        print(f"[Git Protection: merge-gate override] {flat}", file=sys.stderr)
    except OSError:
        pass
    try:
        OVERRIDE_LOG.parent.mkdir(parents=True, exist_ok=True)
        stamp = datetime.now().isoformat(timespec="seconds")
        with OVERRIDE_LOG.open("a") as fh:
            fh.write(f"{stamp} cwd={os.getcwd()} {flat}\n")
        return True
    except OSError:
        return False


def _no_verify_deny_message(command):
    return f"""╔════════════════════════════════════════════════════════════════╗
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
  touch {APPROVAL_FLAG}

Then I'll retry the commit automatically.
Approval expires after 5 minutes and is single-use.

There is no marker that turns this gate off. --no-verify skips
.git/hooks/pre-push, and that hook is the protected-branch guard, so
this gate and the branch gate are one protection with two doors.
The merge-gate override does not reach either of them.

═══════════════════════════════════════════════════════════════════
"""


def _push_deny_message(command, branch, using_no_verify):
    no_verify_warning = ""
    if using_no_verify:
        no_verify_warning = """
⚠️  DETECTED: --no-verify flag
⚠️  This is EXACTLY the behavior we're trying to prevent!
⚠️  Bypassing git hooks is NOT acceptable for protected branches.
"""
    return f"""╔════════════════════════════════════════════════════════════════╗
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

No marker turns this gate off, and the merge-gate override does not
apply here. For an agent session there is deliberately no bypass: push
a branch and open a PR. A human pushing from their own shell is not
gated by this hook at all.

═══════════════════════════════════════════════════════════════════
"""


def _compound_authorization_deny_message(command):
    return f"""╔════════════════════════════════════════════════════════════════╗
║  🚫 BLOCKED: one approval cannot authorize several actions
╚════════════════════════════════════════════════════════════════╝

Command: {command}

An approval here authorizes exactly ONE action, and a PreToolUse allow
applies to the WHOLE Bash call - so `gh pr merge 1 && <anything>` would
approve the anything too. This command carries more than the one action:

  • several `gh pr merge` or `--no-verify` segments, or both kinds
  • another command joined by && / ; / | (a leading `cd` counts too)
  • a command substitution: $(...), `...`, <(...) or >(...)
    A substitution IS a second command, so it disqualifies the
    approval even though it sits inside one segment.
  • an output redirection (>, >>) or an unquoted heredoc (<<EOF),
    which would ride the approval into a file write or an expansion

Run the approved action as its own substitution-free command. To pass a
long commit message without $(cat ...), use stdin instead:

  git commit --no-verify -F - <<'EOF'
  your message
  EOF

═══════════════════════════════════════════════════════════════════
"""


def _unrecordable_override_deny_message():
    return f"""╔════════════════════════════════════════════════════════════════╗
║  🚫 BLOCKED: merge-gate override could not be recorded
╚════════════════════════════════════════════════════════════════╝

The marker was valid but the trail could not be written, and the trail
is what justifies having an override at all - so this is refused rather
than allowed unrecorded. Check that this path is an appendable file (not
a directory) with a writable parent:
  {OVERRIDE_LOG}

The marker was consumed; fix the path, then touch it again:
  touch {MERGE_GATE_MARKER}

═══════════════════════════════════════════════════════════════════
"""


def _merge_deny_message(command):
    return """╔════════════════════════════════════════════════════════════════╗
║  🚫 BLOCKED: gh pr merge (two-factor check failed)
╚════════════════════════════════════════════════════════════════╝

Command: """ + command + f"""

Raw `gh pr merge` is gated at the shipping boundary. The sanctioned merge
primitive is `fno pr merge`, which runs its own footnote-canonical guards
and is not blocked by this hook.

Auto-merge directly from Claude Code requires ALL of:
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

Ahead of the two factors above sits a third veto: review coverage. A bare
`gh pr merge` also requires a `covered` review_coverage row pinned to the
PR's current head. A missing or stale row is refused here even when both
factors pass, because nothing reviewed the head that would merge. The
sanctioned primitive `fno pr merge` recomputes that row itself.

If /pr check was skipped or failed, the correct recovery is to run it
again or explicitly configure --no-external. Do not forge the artifact.

⚠️  Operator escape hatch, when the two-factor path is genuinely broken
    (e.g. an immutable manifest carrying the wrong value):

  touch {MERGE_GATE_MARKER}

    `gh pr merge` only: it cannot open a push to a protected branch and
    cannot open --no-verify. Expires in 5 minutes, consumed on use, every
    use appended to {OVERRIDE_LOG}. An agent must not create this
    unprompted - that is a logged violation, not a silent bypass.

═══════════════════════════════════════════════════════════════════
"""


def _evaluate_git_segment(command, has_approval, allowlist_ok=True):
    """Evaluate one command-position git segment.

    Returns ('deny', reason) | ('allow', reason) | None (safe / no opinion).
    None means the segment is fine (explicitly-allowed git command, a
    feature-branch push, or a bypass-approved protected push).
    """
    # The protected-branch gate is checked FIRST, because it outranks the
    # --no-verify approval. The two are one protection with two doors:
    # --no-verify skips .git/hooks/pre-push, and that hook IS the branch guard,
    # so an approval for one door must never open the other. Checking
    # --no-verify first returned ("allow", ...) for
    # `git push --no-verify origin main` and never reached this check at all -
    # a single segment, so no cross-segment rule can catch it.
    # The allowlist runs FIRST, and it is NOT a no-op: _find_git_segments hands
    # this function a shlex-rejoined argv with QUOTES STRIPPED, so argument text
    # is re-exposed to the regex matchers below. Without it,
    # `git commit -m "fix: block git push origin main"` and
    # `git log --grep "git push origin main"` were denied as pushes to main -
    # a read-only command refused with a wrong explanation. `git push` is not on
    # the allowlist, so a real push still reaches the gate, and
    # `git commit --no-verify` is excluded by the pattern's own lookahead.
    # allowlist_ok is False on the unbalanced-quote fallback, where the "segment"
    # is the WHOLE multi-command string: token[1] there is the first command's
    # subcommand, so `git status && git push origin main 'unbal` was allowlisted
    # by `status` and the push was never evaluated. A fallback that is supposed
    # to be deny-leaning must not consult an allowlist it cannot position.
    if allowlist_ok and is_allowed_git_command(command):
        return None

    is_protected, branch = is_push_to_protected_branch(command)
    if is_protected:
        state = load_state()
        if not (has_recent_approval(state) or check_for_bypass_phrase(state)):
            state["last_blocked_command"] = command
            save_state(state)
            return ("deny", _push_deny_message(command, branch,
                                               is_using_no_verify(command)))
        # Approved for the BRANCH only, so this deliberately falls through to
        # the --no-verify check rather than returning safe. "One approval must
        # not open the other door" has to hold in BOTH directions: returning
        # here let one bypass-phrase push also skip .git/hooks/pre-push and
        # every pre-commit hook.
        print(f"[Git Protection: Approved] Emergency push to {branch}: "
              f"{command}", file=sys.stderr)

    # --no-verify, matched as a WHOLE TOKEN and for ANY subcommand. The old
    # patterns only covered commit/push, so the synthetic flag standing in for a
    # core.hooksPath override (`git config core.hooksPath ...`) never fired. This
    # sits AFTER the branch gate, which outranks it: an approval for the
    # hooks door must never open the branch door, in either direction.
    if any(p.lower() == "--no-verify" for p in command.split()):
        if has_approval:
            return ("allow", "[Approved] User approved --no-verify commit")
        return ("deny", _no_verify_deny_message(command))

    # A git verb this hook does not gate is not this hook's business: it declines
    # to opine and the normal permission system decides.
    return None


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
    if not command:
        sys.exit(0)

    # No pre-gate opt-out marker lives here. One used to, and being ahead of
    # every gate made it an unconditional allow: while it existed, a bare
    # `git push origin main` passed. The merge gate's scoped override is
    # checked inside the merge branch below, where it can only affect merges.

    # Tokenize into command-position segments. On unbalanced quotes shlex
    # raises ValueError; fall back to legacy whole-command matching per gate so
    # a crash never silently drops the gate.
    try:
        segments = _command_segments(command)
    except ValueError:
        segments = None

    # ==========================================
    # gh pr create - always allowed (ad-hoc dev is legit; the merge gate is
    # where pipeline discipline is enforced). gh pr merge - allow only with
    # two-factor (state + artifact) verification.
    # ==========================================
    if segments is not None:
        merge_segs = _find_merge_segments(segments)
        git_segs = _find_git_segments(segments)
    else:
        # legacy fallback (deny-leaning): loose match on the whole command.
        # Deny-leaning for git too: on unbalanced quotes we cannot tell command
        # position from prose, so ANY `git` in the string hands the whole
        # command to the gate. `startswith` was fail-OPEN - an unterminated
        # quote in a command not literally beginning with `git`
        # (`echo "oops<newline>git push origin main`) dropped the push gate
        # entirely, while the merge gate below still fired on the same input.
        # findall, not search: the whole malformed string is one "segment", so a
        # length-1 list would make the multi-merge guard below unfirable on
        # exactly the input this deny-leaning fallback exists to catch.
        merge_segs = [command] * len(re.findall(r'gh\s+pr\s+merge', command,
                                                re.IGNORECASE))
        git_segs = ([command] if re.search(r'\bgit\b', command, re.IGNORECASE)
                    else [])

    merge_seg = merge_segs[0] if merge_segs else None
    has_approval = _fresh_marker(APPROVAL_FLAG)

    # ==========================================
    # Git gates, evaluated per command-position segment so a compound command
    # (`cd /tmp && git push origin main`) can't smuggle a git verb past a
    # startswith('git') check. Evaluated exactly once - a deny writes state -
    # and the verdict is then applied in a fixed precedence below.
    # ==========================================
    # Stops at the first deny: a deny writes state (`last_blocked_command`), so
    # evaluating past it would persist a later segment while the message names
    # the first, making the state file an unreliable record of what was refused.
    git_decisions = []
    for seg in git_segs:
        decision = _evaluate_git_segment(seg, has_approval,
                                         allowlist_ok=segments is not None)
        git_decisions.append((seg, decision))
        if decision is not None and decision[0] == "deny":
            break

    # A DENY ANYWHERE OUTRANKS AN ALLOW ANYWHERE. The loop used to
    # short-circuit on the first decision of either kind, which made the verdict
    # depend on segment order: `git commit --no-verify && git push origin main`
    # returned the approval's allow and carried the push to main with it, while
    # the reverse order denied. One authorization must never cover a sibling
    # segment another gate would refuse - the whole point of scoping the
    # merge-gate marker, applied to every gate rather than just that one.
    for seg, decision in git_decisions:
        if decision is not None and decision[0] == "deny":
            _emit("deny", decision[1])
            sys.exit(0)

    # Which segments, if any, rely on the single-use --no-verify approval.
    # Nothing is CLAIMED yet: a claim before the outcome is known burns the
    # operator's approval on a command that then gets denied anyway.
    git_allows = [(seg, d[1]) for seg, d in git_decisions
                  if d is not None and d[0] == "allow"]
    git_allow = git_allows[0] if git_allows else None

    # ONE AUTHORIZATION AUTHORIZES ONE ACTION. Refuse a command carrying more
    # than one thing a single-use approval would have to cover: several merges
    # rode one marker consume (logging only the first), several approved
    # --no-verify segments rode one flag consume, and an approved --no-verify
    # segment beside a merge would need two independent single-use claims
    # committed atomically within one tool call. All are the compound-smuggling
    # shape this gate exists to refuse, so they are refused rather than
    # half-honored. Keeping them mutually exclusive is also what lets each
    # branch below claim without a two-phase commit.
    if (len(merge_segs) > 1 or len(git_allows) > 1
            or (merge_seg is not None and git_allow is not None)):
        _emit("deny", _compound_authorization_deny_message(command))
        sys.exit(0)

    # A PreToolUse allow is NOT segment-scoped: it approves the whole Bash call.
    # So anything this hook AUTHORIZES must stand alone, or the authorization
    # silently covers its siblings - `gh pr merge 1 && gh api -X PATCH
    # .../refs/heads/main` and `git commit --no-verify -m x && rm -rf ...` were
    # each allowed whole, and neither sibling is a git segment any gate
    # inspects. Applied to EVERY authorizing path, including the two-factor
    # merge: a leading `cd` is deliberately not carved out, because the allow
    # covers a prefix exactly as it covers a suffix, so tolerating one reopens
    # the hole from the other side. Only a DENY may be emitted for a compound
    # command. Verified: no production caller compounds `gh pr merge` through
    # the Bash tool - the fno/gh call sites all go through subprocess, which
    # this hook never sees.
    # A `$(...)` body is re-segmented and so already trips the count, but
    # _substitution_bodies deliberately skips backticks, and `<(`/`>(` are not
    # separators, so both stay inside ONE segment. That made the rule decorative
    # on two live forms: `gh pr merge 12 --body "`id > /tmp/pwn`"` counted as a
    # lone command and ran the backtick body under the override. Any
    # substitution form disqualifies an authorization - a command substitution
    # IS a second command, whatever its delimiter.
    # Scanned over the parsed TOKENS, not the raw command. A raw substring scan
    # also matched a backtick inside a heredoc body, which broke the very
    # `-F - <<'EOF'` escape the refusal message recommends - a markdown code span
    # in a commit message was enough. Tokens exclude heredoc bodies, so the
    # quoted-delimiter form works while a double-quoted or unquoted `$(...)` /
    # backtick still lands in a token and is caught.
    #
    # A `>` disqualifies too: an authorization covers the whole Bash call, so
    # `git commit --no-verify -m ok > /tmp/log` also approved an arbitrary file
    # overwrite that no gate inspected. `<` is deliberately NOT listed - input
    # redirection cannot run a command, and `<<` is the recommended escape.
    tokens = [tok for seg in (segments or []) for tok in seg]
    carries_substitution = (
        any(f in tok for tok in tokens for f in _SUBSTITUTION_FORMS)
        or _writes_a_file(tokens)
        or _has_unquoted_heredoc(command))
    # Only enforceable when shlex actually parsed the command. On the
    # unbalanced-quote fallback there is one pseudo-segment, so requiring
    # len == 1 there refused EVERY fallback merge before the two-factor check
    # ran - a routine apostrophe in `--body "it's ready"` was enough to block a
    # legitimate auto-merge, with a wrong reason. The two-factor path keeps its
    # prior behaviour there; the marker override does not, since it needs parsed
    # segments to prove the merge stands alone (see its own guard below).
    # A shell runner disqualifies outright: _effective_argv re-tokenizes its
    # quoted argument so the inner verb IS gated, but the outer command is still
    # ONE segment, so `bash -c "gh pr merge 42 && rm -rf /tmp/x"` counted as
    # standing alone while the unwrapped form was refused.
    wrapped = bool(_SHELL_RUNNER_RE.search(command))
    if segments is None:
        # Nothing can be counted on the unparseable fallback, so nothing may be
        # authorized there. The merge gate keeps its own path (it is
        # evidence-backed and pre-existing); a single-use approval does not.
        if git_allow is not None:
            _emit("deny", _compound_authorization_deny_message(command))
            sys.exit(0)
    else:
        authorizes_alone = (len(segments) == 1 and not carries_substitution
                            and not wrapped)
        if (merge_seg is not None or git_allow is not None) and not authorizes_alone:
            _emit("deny", _compound_authorization_deny_message(command))
            sys.exit(0)

    if merge_seg is not None:
        # Checked BEFORE the two-factor path, so it vetoes every route that
        # would otherwise allow - including the merge-gate override marker.
        # That marker buys out the review ceremony; a base that no longer leads
        # to the default branch is not a ceremony, it is a merge that ships
        # nothing. The lineage check carries its own documented bypass
        # (FNO_PR_BASE_LINEAGE_OK), which the CLI verb honours by exiting 0.
        stacked = _stacked_base_refusal(merge_seg)
        if stacked:
            _emit("deny", f"[fno stacked-base] {stacked}")
            sys.exit(0)
        # Beside the lineage veto and ahead of the two-factor allow for the
        # same reason: the override marker buys out review ceremony, and a PR
        # nothing reviewed at the head that would merge is not ceremony. The
        # recovery line wraps the guard's own sentence verbatim - never edits
        # it - so the hook's reason and `fno pr merge`'s receipt carry one
        # recognizable sentence between them.
        covref = _coverage_refusal(merge_seg)
        if covref:
            _emit("deny", f"[fno review-coverage] {covref}\n"
                          "Recovery: run `fno pr merge`, which recomputes coverage "
                          "and is not gated by this hook.")
            sys.exit(0)
        allow_reason = _check_pr_merge_allowed(merge_seg)
        if allow_reason:
            _emit("allow", f"[fno auto-merge] {allow_reason}")
            sys.exit(0)
        # Scoped override, reached only after the legitimate two-factor path
        # failed and every git segment cleared, so the marker is claimed only
        # when it is actually the thing authorizing the merge.
        # `segments is not None` is load-bearing: the lone-command rule above is
        # skipped on the unparseable fallback, so without it the override could
        # authorize a command whose siblings were never counted.
        if (segments is not None and _fresh_marker(MERGE_GATE_MARKER)
                and _claim_marker(MERGE_GATE_MARKER)):
            if not _audit("two-factor check failed, merge allowed by "
                          f"marker: {merge_seg}"):
                _emit("deny", _unrecordable_override_deny_message())
                sys.exit(0)
            _emit("allow", "[fno merge-gate override] marker consumed; "
                           "recorded in merge-gate-overrides.log")
            sys.exit(0)
        _emit("deny", _merge_deny_message(merge_seg))
        sys.exit(0)

    if git_allow is not None:
        # The claim must WIN, not merely find a fresh flag: has_approval above
        # is a plain stat, so two concurrent hooks both see it and only the one
        # that removes it is authorized.
        if not _claim_marker(APPROVAL_FLAG):
            _emit("deny", _no_verify_deny_message(git_allow[0]))
            sys.exit(0)
        _emit("allow", git_allow[1])
        sys.exit(0)

    # All git segments safe, allow it
    sys.exit(0)


if __name__ == "__main__":
    main()
