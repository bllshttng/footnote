#!/usr/bin/env bash
# Tests for the location pre-flight gate at the top of
# hooks/helpers/init-target-state.sh (backlog ab-efcde945).
#
# Verifies the gate refuses canonical-main, allows worktrees, allows
# canonical feature branches, honors TARGET_LOCATION_OK consent, and
# is a no-op outside a git repo.
#
# Each test scenario creates an isolated REPO_ROOT (or worktree) under
# a tempdir, invokes init-target-state.sh with TARGET_START=1, and asserts
# on exit code + state-file presence + stderr message.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INIT_SCRIPT="$REPO_ROOT/hooks/helpers/init-target-state.sh"

if [[ ! -f "$INIT_SCRIPT" ]]; then
    echo "FAIL: init-target-state.sh not found at $INIT_SCRIPT" >&2
    exit 1
fi

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP_BASE="$(mktemp -d -t target-init-loc-XXXXXX)"
trap 'rm -rf "$TMP_BASE"' EXIT

# Each scenario gets a fresh repo so they're independent. CLAUDE_PLUGIN_ROOT
# points to REPO_ROOT so the script can find scripts/lib/config.sh during
# the post-init phase that runs after the gate.
make_repo() {
    local dir="$1"
    local branch="$2"
    mkdir -p "$dir"
    (
        cd "$dir"
        git init -q -b "$branch" 2>/dev/null || {
            git init -q
            git checkout -q -b "$branch"
        }
        git config user.email "test@test.com"
        git config user.name "Test"
        echo "# test" > README.md
        git add README.md
        git commit -q -m "init"
    )
}

# Run init-target-state.sh isolated from caller env that could mask the gate.
# Extra KEY=VAL pairs may be passed as positional args after $cwd; they are
# forwarded to `env` before the script runs. Avoids BSD/GNU `env --` skew.
run_init() {
    local cwd="$1"
    shift
    (
        cd "$cwd"
        # Strip any pre-existing trigger/consent state to make scenarios deterministic.
        unset TARGET_START TARGET_INPUT TARGET_PLAN_PATH TARGET_LOCATION_OK TARGET_SIZE
        env TARGET_START=1 CLAUDE_PLUGIN_ROOT="$REPO_ROOT" "$@" bash "$INIT_SCRIPT" 2>&1
    )
    return $?
}

echo "=== test-init-location-gate (ab-efcde945) ==="

# --- AC1: canonical checkout on main REFUSED without consent ---------------
echo ""
echo "--- AC1: canonical + main refuses ---"
T="$TMP_BASE/ac1-canonical-main"
make_repo "$T" "main"
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -ne 0 ]]; then
    pass "AC1: exit code non-zero on canonical main ($EC)"
else
    fail "AC1: expected non-zero exit on canonical main, got 0"
fi
if echo "$OUT" | grep -q "REFUSED: cwd is the canonical checkout on branch 'main'"; then
    pass "AC1: refusal message present"
else
    fail "AC1: refusal message missing. Got: $OUT"
fi
if [[ ! -f "$T/.fno/target-state.md" ]]; then
    pass "AC1: target-state.md NOT created"
else
    fail "AC1: target-state.md was created despite refusal"
fi

# --- AC2: canonical checkout on master REFUSED without consent -------------
echo ""
echo "--- AC2: canonical + master refuses ---"
T="$TMP_BASE/ac2-canonical-master"
make_repo "$T" "master"
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -ne 0 ]]; then
    pass "AC2: exit code non-zero on canonical master ($EC)"
else
    fail "AC2: expected non-zero exit on canonical master, got 0"
fi
if echo "$OUT" | grep -q "REFUSED: cwd is the canonical checkout on branch 'master'"; then
    pass "AC2: refusal message names 'master'"
else
    fail "AC2: refusal message missing. Got: $OUT"
fi

# --- AC3: canonical checkout on feature branch ALLOWED --------------------
echo ""
echo "--- AC3: canonical + feature branch allowed ---"
T="$TMP_BASE/ac3-canonical-feature"
make_repo "$T" "feature/some-work"
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC3: exit 0 on canonical feature branch"
else
    fail "AC3: expected exit 0 on canonical feature branch, got $EC. Output: $OUT"
fi
if [[ -f "$T/.fno/target-state.md" ]]; then
    pass "AC3: target-state.md created"
else
    fail "AC3: target-state.md missing after successful init"
fi
if ! echo "$OUT" | grep -q "REFUSED"; then
    pass "AC3: no refusal message on feature branch"
else
    fail "AC3: unexpected refusal on feature branch. Got: $OUT"
fi

# --- AC4: worktree ALLOWED regardless of branch name -----------------------
echo ""
echo "--- AC4: worktree on 'main'-like name allowed ---"
CANON="$TMP_BASE/ac4-canonical"
WT="$TMP_BASE/ac4-worktree"
make_repo "$CANON" "main"
(
    cd "$CANON"
    # Create a worktree on a new branch named 'main-like' to prove the
    # gate keys off the .git-is-a-file detection, not the branch name.
    git worktree add -q "$WT" -b worktree-branch 2>/dev/null
) || fail "AC4: worktree creation failed"
if [[ -f "$WT/.git" && ! -d "$WT/.git" ]]; then
    pass "AC4: worktree .git is a gitfile (not a dir) — detection precondition holds"
else
    fail "AC4: worktree .git layout unexpected (expected file, not dir)"
fi
OUT=$(run_init "$WT" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC4: exit 0 inside worktree"
else
    fail "AC4: expected exit 0 inside worktree, got $EC. Output: $OUT"
fi
if [[ -f "$WT/.fno/target-state.md" ]]; then
    pass "AC4: worktree state file created"
else
    fail "AC4: worktree state file missing"
fi

# --- AC5: TARGET_LOCATION_OK=main-acknowledged ALLOWS canonical main --------
echo ""
echo "--- AC5: explicit consent allows canonical main ---"
T="$TMP_BASE/ac5-consent"
make_repo "$T" "main"
OUT=$(run_init "$T" TARGET_LOCATION_OK=main-acknowledged 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC5: exit 0 with consent env"
else
    fail "AC5: expected exit 0 with consent env, got $EC. Output: $OUT"
fi
if echo "$OUT" | grep -q "WARNING: proceeding on canonical 'main'"; then
    pass "AC5: warning shown when consent honored"
else
    fail "AC5: expected warning message. Got: $OUT"
fi
if [[ -f "$T/.fno/target-state.md" ]]; then
    pass "AC5: state file created under explicit consent"
else
    fail "AC5: state file missing under explicit consent"
fi

# --- AC6: other consent values do NOT bypass (only main-acknowledged) -----
echo ""
echo "--- AC6: invalid consent values still refused ---"
T="$TMP_BASE/ac6-bad-consent"
make_repo "$T" "main"
OUT=$(run_init "$T" TARGET_LOCATION_OK=yes 2>&1)
EC=$?
if [[ $EC -ne 0 ]]; then
    pass "AC6: invalid consent ('yes') still refused"
else
    fail "AC6: invalid consent ('yes') should be refused, got exit 0"
fi

# --- AC7: non-git directory is a no-op (legacy compat) ---------------------
echo ""
echo "--- AC7: non-git directory passes through ---"
T="$TMP_BASE/ac7-not-a-repo"
mkdir -p "$T"
OUT=$(run_init "$T" 2>&1)
EC=$?
# Outside a repo, REPO_ROOT falls back to $(pwd) (line 16 of init script),
# which has no .git — so the location gate short-circuits and lets the
# script proceed. The script then writes target-state.md into the tempdir.
if [[ $EC -eq 0 ]]; then
    pass "AC7: non-git dir allowed"
else
    fail "AC7: non-git dir should pass, got exit $EC. Output: $OUT"
fi
if ! echo "$OUT" | grep -q "REFUSED"; then
    pass "AC7: no refusal in non-git dir"
else
    fail "AC7: unexpected refusal in non-git dir. Got: $OUT"
fi

# --- AC8: refusal output names the canonical checkout path ----------------
echo ""
echo "--- AC8: refusal message includes actionable worktree command ---"
T="$TMP_BASE/ac8-message-shape"
make_repo "$T" "main"
OUT=$(run_init "$T" 2>&1)
if echo "$OUT" | grep -q "git worktree add"; then
    pass "AC8: refusal includes 'git worktree add' command"
else
    fail "AC8: refusal should suggest 'git worktree add'. Got: $OUT"
fi
if echo "$OUT" | grep -q "git checkout -b feature/"; then
    pass "AC8: refusal includes 'git checkout -b feature/' option"
else
    fail "AC8: refusal should suggest a feature branch path. Got: $OUT"
fi
if echo "$OUT" | grep -q "TARGET_LOCATION_OK=main-acknowledged"; then
    pass "AC8: refusal documents the consent env"
else
    fail "AC8: refusal should document TARGET_LOCATION_OK. Got: $OUT"
fi
if echo "$OUT" | grep -q "ab-efcde945"; then
    pass "AC8: refusal cites backlog node for context"
else
    fail "AC8: refusal should cite ab-efcde945. Got: $OUT"
fi

# --- AC8b: unknown-branch (rev-parse fails for non-unborn reason) refused -
# Codex round 4 P1: the unborn discriminator must distinguish "truly fresh
# repo" from "rev-parse failed for some other reason" (dubious ownership,
# corrupted refs, permission errors). The original `rev-parse HEAD || HAS=0`
# check was too broad — it would wave canonical main through under any
# rev-parse failure. Fix uses a positive unborn signal via symbolic-ref.
#
# Simulate the "unknown branch" state by corrupting .git/HEAD so both
# symbolic-ref and rev-parse fail without indicating unborn. This is
# closest to what dubious-ownership produces in practice (commands fail
# without leaving the repo in an obviously-fresh state).
echo ""
echo "--- AC8b: unknown-branch state refused (not waved through as unborn) ---"
T="$TMP_BASE/ac8b-unknown-branch"
make_repo "$T" "main"
# Corrupt HEAD: write garbage that's neither a ref pointer nor a sha.
# symbolic-ref returns non-zero, rev-parse HEAD returns non-zero, but
# the repo otherwise has commits — this is decisively NOT "fresh".
echo "totally-not-a-valid-head" > "$T/.git/HEAD"
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -ne 0 ]]; then
    pass "AC8b: exit non-zero when branch cannot be determined ($EC)"
else
    fail "AC8b: expected non-zero exit on unknown branch, got 0. Output: $OUT"
fi
if echo "$OUT" | grep -q "unknown branch"; then
    pass "AC8b: refusal message names the unknown-branch state"
else
    fail "AC8b: refusal should mention unknown branch. Got: $OUT"
fi
if echo "$OUT" | grep -q "safe.directory"; then
    pass "AC8b: refusal suggests the safe.directory fix"
else
    fail "AC8b: refusal should hint at safe.directory. Got: $OUT"
fi
if [[ ! -f "$T/.fno/target-state.md" ]]; then
    pass "AC8b: target-state.md NOT created in unknown-branch state"
else
    fail "AC8b: target-state.md was created despite refusal"
fi

# --- AC9: detached HEAD refused (no branch) -------------------------------
echo ""
echo "--- AC9: canonical + detached HEAD refused ---"
T="$TMP_BASE/ac9-detached"
make_repo "$T" "main"
# Detach HEAD onto the initial commit so `rev-parse --abbrev-ref HEAD` returns 'HEAD'.
(cd "$T" && git checkout -q --detach HEAD)
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -ne 0 ]]; then
    pass "AC9: exit non-zero on detached HEAD ($EC)"
else
    fail "AC9: expected non-zero exit on detached HEAD, got 0. Output: $OUT"
fi
if echo "$OUT" | grep -q "detached HEAD"; then
    pass "AC9: refusal message names 'detached HEAD'"
else
    fail "AC9: refusal should mention detached HEAD. Got: $OUT"
fi
if [[ ! -f "$T/.fno/target-state.md" ]]; then
    pass "AC9: target-state.md NOT created in detached state"
else
    fail "AC9: target-state.md was created in detached state"
fi

# --- AC10: detached HEAD allowed with consent env -------------------------
echo ""
echo "--- AC10: detached HEAD + consent allowed ---"
T="$TMP_BASE/ac10-detached-consent"
make_repo "$T" "main"
(cd "$T" && git checkout -q --detach HEAD)
OUT=$(run_init "$T" TARGET_LOCATION_OK=main-acknowledged 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC10: exit 0 with consent on detached HEAD"
else
    fail "AC10: expected exit 0 with consent, got $EC. Output: $OUT"
fi
if [[ -f "$T/.fno/target-state.md" ]]; then
    pass "AC10: state file created under consent"
else
    fail "AC10: state file missing despite consent"
fi

# --- AC11: script contains 'unset TARGET_LOCATION_OK' (sticky env defense) -
echo ""
echo "--- AC11: consent env consumed after gate ---"
if grep -q "^unset TARGET_LOCATION_OK" "$INIT_SCRIPT"; then
    pass "AC11: init script unsets TARGET_LOCATION_OK after consuming"
else
    fail "AC11: init script missing 'unset TARGET_LOCATION_OK' (sticky-env defense for child processes)"
fi

# --- AC11b: --separate-git-dir clone treated as canonical (not worktree) --
# Codex finding (PR #321 round 2): the naive ".git is a file = worktree"
# detection mis-classifies setups where .git is a gitfile for reasons OTHER
# than git worktree, specifically submodules and `--separate-git-dir`
# clones. Both of those have their own single working tree and the same
# cross-terminal pollution risk as canonical. The fix uses
# `git rev-parse --git-dir` vs `--git-common-dir`: they DIFFER for linked
# worktrees only.
echo ""
echo "--- AC11b: --separate-git-dir on main is refused (not waved through) ---"
T="$TMP_BASE/ac11b-sep-gitdir"
SEP_GITDIR="$TMP_BASE/ac11b-real-gitdir"
mkdir -p "$T" "$SEP_GITDIR"
# `git init --separate-git-dir <D> .` puts the gitdir at D and writes a
# `.git` GITFILE pointing at it. The working tree is at the cwd of `git init`.
(cd "$T" && git init -q --separate-git-dir "$SEP_GITDIR" 2>/dev/null \
    && git config user.email "test@test.com" \
    && git config user.name "Test" \
    && echo "# x" > README.md && git add README.md && git commit -q -m init)
if [[ -f "$T/.git" && ! -d "$T/.git" ]]; then
    pass "AC11b: .git is a gitfile (precondition for the test)"
else
    fail "AC11b: --separate-git-dir setup unexpected (expected gitfile)"
fi
# Make sure the branch is `main` (init.defaultBranch may vary in CI).
(cd "$T" && git branch -m main 2>/dev/null || true)
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -ne 0 ]]; then
    pass "AC11b: --separate-git-dir on main is REFUSED (no longer waved through as worktree)"
else
    fail "AC11b: --separate-git-dir on main was incorrectly allowed (exit 0). Output: $OUT"
fi
if [[ ! -f "$T/.fno/target-state.md" ]]; then
    pass "AC11b: target-state.md NOT created in --separate-git-dir clone on main"
else
    fail "AC11b: target-state.md was created in --separate-git-dir clone on main"
fi

# --- AC12: fresh git init with no commits passes through ------------------
# Existing test fixtures (test_size_profile, test_init_provenance, etc.)
# do `git init -q` without committing. `git rev-parse --abbrev-ref HEAD`
# returns 'HEAD' in that state — same string as real detached HEAD. The
# gate must distinguish them by checking whether HEAD resolves to a sha.
echo ""
echo "--- AC12: no-commit fresh repo allowed ---"
T="$TMP_BASE/ac12-no-commits"
mkdir -p "$T"
(cd "$T" && git init -q)  # No commit; matches the common test-fixture pattern.
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC12: no-commit repo passes the gate"
else
    fail "AC12: expected exit 0 on fresh-no-commits, got $EC. Output: $OUT"
fi
if [[ -f "$T/.fno/target-state.md" ]]; then
    pass "AC12: state file created on no-commit fresh init"
else
    fail "AC12: state file missing on no-commit fresh init. Output: $OUT"
fi
if ! echo "$OUT" | grep -q "REFUSED"; then
    pass "AC12: no refusal message on no-commit fresh init"
else
    fail "AC12: unexpected refusal on no-commit fresh init. Got: $OUT"
fi

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]]
