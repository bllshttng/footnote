#!/usr/bin/env bash
# Tests for the session-aware gh pr merge gate in git-protection.py.
# Exercises the REPO copy of the hook (not any user-global install) with a
# temp HOME + temp FNO_HOME + temp repo for full isolation. Real ~/.fno and
# ~/.claude are never touched; green on a machine with no ~/.claude/hooks/.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/git-protection.py"
PASS=0
FAIL=0

# ---- prereq checks ----

if [[ ! -f "$HOOK" ]]; then
    echo "SKIP: $HOOK not found"
    exit 0
fi

if ! command -v python3 &>/dev/null; then
    echo "SKIP: python3 not installed"
    exit 0
fi

# ---- test helpers ----

pass() { echo "  PASS: $1"; ((PASS++)); }
fail() { echo "  FAIL: $1"; ((FAIL++)); }

assert_allow() {
    local desc="$1" result="$2"
    if echo "$result" | grep -qE '"permissionDecision":\s*"allow"'; then
        pass "$desc"
    else
        fail "$desc (expected allow, got: $result)"
    fi
}

assert_deny() {
    local desc="$1" result="$2"
    if echo "$result" | grep -qE '"permissionDecision":\s*"deny"'; then
        pass "$desc"
    else
        fail "$desc (expected deny, got: $result)"
    fi
}

# ---- sandbox setup ----

TMP=$(mktemp -d)
trap "rm -rf '$TMP'" EXIT

# Sandbox HOME and FNO_HOME so the hook's state (git-protection.json,
# approve_no_verify.flag) resolves under the temp dir, never the host.
export HOME="$TMP"
export FNO_HOME="$TMP/.fno"
mkdir -p "$FNO_HOME"

# Create a temp git repo so git rev-parse works
REPO="$TMP/repo"
mkdir -p "$REPO/.fno"
cd "$REPO"
git init -q

# Helper: send a Bash tool call through the hook
run_hook() {
    local command="$1"
    echo "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"$command\"}}" \
        | python3 "$HOOK"
}

# ---- Case 1: no state file, gh pr merge denied ----

result=$(run_hook "gh pr merge 42 --merge")
assert_deny "case 1: no state file -> deny" "$result"

# ---- Case 2: fresh state file, approved + external skipped -> allowed ----
# Two-factor: auto_merge_approved (factor 1) + external_review_passed: skipped
# (factor 2a, the --no-external path) authorizes without an artifact.

cat > "$REPO/.fno/target-state.md" <<'STATE'
---
status: IN_PROGRESS
auto_merge_approved: true
external_review_passed: skipped
---
STATE

result=$(run_hook "gh pr merge 42 --merge")
assert_allow "case 2: approved + external skipped -> allow" "$result"

# ---- Case 3: state file with auto_merge_approved: false -> denied ----

cat > "$REPO/.fno/target-state.md" <<'STATE'
---
status: IN_PROGRESS
auto_merge_approved: false
---
STATE

result=$(run_hook "gh pr merge 42 --merge")
assert_deny "case 3: auto_merge_approved: false -> deny" "$result"

# ---- Case 4: state file with approval but 2 hours old -> denied (stale) ----

cat > "$REPO/.fno/target-state.md" <<'STATE'
---
auto_merge_approved: true
---
STATE
# Push mtime back 2 hours - macOS touch -A or GNU touch -d
touch -A -020000 "$REPO/.fno/target-state.md" 2>/dev/null || \
    touch -d "2 hours ago" "$REPO/.fno/target-state.md"

result=$(run_hook "gh pr merge 42 --merge")
assert_deny "case 4: stale state file (2h old) -> deny" "$result"

# Clean up state file for regression tests
rm -f "$REPO/.fno/target-state.md"

# ---- Regression 1: git commit --no-verify still denied ----

result=$(run_hook "git commit --no-verify -m foo")
assert_deny "regression 1: git commit --no-verify -> deny" "$result"

# ---- Regression 2: git push origin main still denied ----

result=$(run_hook "git push origin main")
assert_deny "regression 2: git push origin main -> deny" "$result"

# ---- Case 5: megawalk-state.md does NOT authorize a merge -> denied ----
# Megawalk orchestrates target subagents; only target's own target-state.md
# authorizes shipping. A megawalk-state.md, even approved, must not merge.

cat > "$REPO/.fno/megawalk-state.md" <<'STATE'
---
status: LOOPING
auto_merge_approved: true
---
STATE

result=$(run_hook "gh pr merge 42 --merge")
assert_deny "case 5: megawalk-state.md does not authorize merge -> deny" "$result"
rm -f "$REPO/.fno/megawalk-state.md"

# ---- Case 6: COMPLETE status with auto_merge_approved: true -> denied ----

cat > "$REPO/.fno/target-state.md" <<'STATE'
---
status: COMPLETE
auto_merge_approved: true
---
STATE

result=$(run_hook "gh pr merge 42 --merge")
assert_deny "case 6: COMPLETE status -> deny" "$result"
rm -f "$REPO/.fno/target-state.md"

# ---- Case 7: body contains auto_merge_approved: true but frontmatter does not -> denied ----

cat > "$REPO/.fno/target-state.md" <<'STATE'
---
status: IN_PROGRESS
---

Some log content below the frontmatter.
auto_merge_approved: true # this is in the body, not frontmatter
STATE

result=$(run_hook "gh pr merge 42 --merge")
assert_deny "case 7: auto_merge_approved in body only -> deny" "$result"
rm -f "$REPO/.fno/target-state.md"

# ---- Case 8: auto_merge_approved: true + status: COMPLETE -> denied ----

cat > "$REPO/.fno/target-state.md" <<'STATE'
---
status: COMPLETE
auto_merge_approved: true
---
STATE

result=$(run_hook "gh pr merge 42 --merge")
assert_deny "case 8: terminal status COMPLETE + approved -> deny" "$result"
rm -f "$REPO/.fno/target-state.md"

# ---- Case 9: corrupt/binary state file -> hook must not crash and must deny ----

# Write a file with binary-like content (null bytes etc.)
printf -- '---\nstatus: IN_PROGRESS\n---\n\x00\x01\x02 corrupt data' > "$REPO/.fno/target-state.md"

result=$(run_hook "gh pr merge 42 --merge")
assert_deny "case 9: corrupt state file -> deny (no crash)" "$result"
rm -f "$REPO/.fno/target-state.md"

# ---- Case 10: approved + external skipped + IN_PROGRESS -> allowed ----

cat > "$REPO/.fno/target-state.md" <<'STATE'
---
status: IN_PROGRESS
auto_merge_approved: true
external_review_passed: skipped
---
STATE

result=$(run_hook "gh pr merge 42 --merge")
assert_allow "case 10: IN_PROGRESS + approved + external skipped -> allow" "$result"
rm -f "$REPO/.fno/target-state.md"

# ---- Summary ----

echo ""
TOTAL=$((PASS + FAIL))
echo "PASS: $PASS/$TOTAL"

if [[ $FAIL -gt 0 ]]; then
    echo "FAIL: $FAIL tests failed"
    exit 1
fi
