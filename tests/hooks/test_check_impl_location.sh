#!/usr/bin/env bash
# test_check_impl_location.sh
#
# Unit tests for the shared location verdict helper
# (hooks/helpers/check-impl-location.sh), the single source of truth consumed
# by /target, /do, /fix, init-target-state.sh, and the SessionStart heads-up
# (design: Worktree Scope Hygiene).
#
# Verifies the verdict scalars (verdict / is_canonical / branch / is_unborn)
# and the nested-worktree advisory (nested_count / nested_path), and that the
# helper ALWAYS exits 0 and degrades safely outside a git repo.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HELPER="$REPO_ROOT/hooks/helpers/check-impl-location.sh"

if [[ ! -f "$HELPER" ]]; then
    echo "FAIL: helper not found at $HELPER" >&2
    exit 1
fi

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP_BASE="$(mktemp -d -t check-impl-loc-XXXXXX)"
trap 'rm -rf "$TMP_BASE"' EXIT

make_repo() {
    local dir="$1" branch="$2"
    mkdir -p "$dir"
    (
        cd "$dir"
        git init -q -b "$branch" 2>/dev/null || { git init -q; git checkout -q -b "$branch"; }
        git config user.email t@t.com
        git config user.name Test
        echo "# x" > README.md
        git add README.md
        git commit -q -m init
    )
}

# Run the helper from a cwd and echo a single key's value.
val() { printf '%s\n' "$1" | sed -n "s/^${2}=//p" | head -1; }

run_helper() { ( cd "$1" && bash "$HELPER" ); }

echo "=== test_check_impl_location ==="

# --- canonical + main -> canonical-protected -------------------------------
echo ""
echo "--- canonical main ---"
T="$TMP_BASE/canon-main"; make_repo "$T" main
OUT="$(run_helper "$T")"; RC=$?
[[ $RC -eq 0 ]] && pass "exit 0" || fail "expected exit 0, got $RC"
[[ "$(val "$OUT" verdict)" == "canonical-protected" ]] && pass "verdict=canonical-protected" || fail "verdict was '$(val "$OUT" verdict)'"
[[ "$(val "$OUT" is_canonical)" == "1" ]] && pass "is_canonical=1" || fail "is_canonical was '$(val "$OUT" is_canonical)'"
[[ "$(val "$OUT" branch)" == "main" ]] && pass "branch=main" || fail "branch was '$(val "$OUT" branch)'"

# --- canonical + feature branch -> ok --------------------------------------
echo ""
echo "--- canonical feature branch ---"
T="$TMP_BASE/canon-feat"; make_repo "$T" feature/widget
OUT="$(run_helper "$T")"
[[ "$(val "$OUT" verdict)" == "ok" ]] && pass "verdict=ok on feature branch" || fail "verdict was '$(val "$OUT" verdict)'"
[[ "$(val "$OUT" is_canonical)" == "1" ]] && pass "is_canonical=1 on canonical feature" || fail "is_canonical was '$(val "$OUT" is_canonical)'"

# --- linked worktree -> ok regardless of branch name -----------------------
echo ""
echo "--- linked worktree ---"
CANON="$TMP_BASE/wt-canon"; WT="$TMP_BASE/wt-linked"; make_repo "$CANON" main
( cd "$CANON" && git worktree add -q "$WT" -b some-work )
OUT="$(run_helper "$WT")"
[[ "$(val "$OUT" verdict)" == "ok" ]] && pass "verdict=ok inside linked worktree" || fail "verdict was '$(val "$OUT" verdict)'"
[[ "$(val "$OUT" is_canonical)" == "0" ]] && pass "is_canonical=0 inside linked worktree" || fail "is_canonical was '$(val "$OUT" is_canonical)'"

# --- unborn (no commits) -> ok + is_unborn=1 -------------------------------
echo ""
echo "--- unborn fresh repo ---"
T="$TMP_BASE/unborn"; mkdir -p "$T"; ( cd "$T" && git init -q )
OUT="$(run_helper "$T")"
[[ "$(val "$OUT" verdict)" == "ok" ]] && pass "verdict=ok on unborn repo" || fail "verdict was '$(val "$OUT" verdict)'"
[[ "$(val "$OUT" is_unborn)" == "1" ]] && pass "is_unborn=1 on unborn repo" || fail "is_unborn was '$(val "$OUT" is_unborn)'"

# --- non-git directory -> ok (degrade) -------------------------------------
echo ""
echo "--- non-git directory ---"
T="$TMP_BASE/not-a-repo"; mkdir -p "$T"
OUT="$(run_helper "$T")"; RC=$?
[[ $RC -eq 0 ]] && pass "exit 0 outside git repo" || fail "expected exit 0, got $RC"
[[ "$(val "$OUT" verdict)" == "ok" ]] && pass "verdict=ok outside git repo" || fail "verdict was '$(val "$OUT" verdict)'"
[[ "$(val "$OUT" nested_count)" == "0" ]] && pass "nested_count=0 outside git repo" || fail "nested_count was '$(val "$OUT" nested_count)'"

# --- nested worktree under .claude/worktrees/ -> flagged from canonical -----
echo ""
echo "--- nested worktree present (canonical) ---"
T="$TMP_BASE/nested-canon"; make_repo "$T" feature/safe
( cd "$T" && git worktree add -q "$T/.claude/worktrees/stray" -b stray-branch )
OUT="$(run_helper "$T")"
[[ "$(val "$OUT" nested_count)" == "1" ]] && pass "nested_count=1 from canonical with a nested worktree" || fail "nested_count was '$(val "$OUT" nested_count)'"
if printf '%s\n' "$OUT" | grep -q "^nested_path=.*/.claude/worktrees/stray$"; then
    pass "nested_path names the offending worktree"
else
    fail "nested_path missing/wrong. Output: $OUT"
fi

# --- nested worktree NOT flagged from inside that worktree (clean view) -----
echo ""
echo "--- nested worktree: silent from a sibling clean worktree ---"
SIB="$TMP_BASE/sibling-clean"
( cd "$T" && git worktree add -q "$SIB" -b sibling-clean )
OUT="$(run_helper "$SIB")"
[[ "$(val "$OUT" nested_count)" == "0" ]] && pass "nested_count=0 from a worktree with no .claude/worktrees/ under it" || fail "nested_count was '$(val "$OUT" nested_count)'"

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]]
