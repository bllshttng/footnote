#!/usr/bin/env bash
# Tests for the terminal-state archival behavior added to
# init-target-state.sh (ab-efcde945 follow-on, stale-COMPLETE silent-no-op
# fix).
#
# When TARGET_START=1 fires (skill body is starting a NEW session) and the
# state file exists with status COMPLETE / BLOCKED / ABORTED, the file is
# archived under a timestamped name and the fresh-init branch runs.
# IN_PROGRESS is preserved (genuine resume case).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INIT_SCRIPT="$REPO_ROOT/hooks/helpers/init-target-state.sh"

[[ -f "$INIT_SCRIPT" ]] || { echo "FAIL: $INIT_SCRIPT missing" >&2; exit 1; }

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP_BASE="$(mktemp -d -t target-init-terminal-XXXXXX)"
trap 'rm -rf "$TMP_BASE"' EXIT

make_repo() {
    local dir="$1"
    local branch="$2"
    mkdir -p "$dir"
    (
        cd "$dir"
        git init -q -b "$branch" 2>/dev/null || { git init -q; git checkout -q -b "$branch"; }
        git config user.email "test@test.com"
        git config user.name "Test"
        echo "# test" > README.md
        git add README.md
        git commit -q -m "init"
    )
}

# Plant a state file with the given status in $dir/.fno/target-state.md.
plant_state() {
    local dir="$1"
    local status="$2"
    mkdir -p "$dir/.fno"
    cat > "$dir/.fno/target-state.md" <<EOF
---
status: $status
session_id: planted-fixture-20260522
created_at: 2026-05-21T00:00:00Z
current_phase: planted
iteration: 1
---

# Planted state file for terminal-state archive test
EOF
}

run_init() {
    local cwd="$1"
    shift
    (
        cd "$cwd"
        unset TARGET_START TARGET_INPUT TARGET_PLAN_PATH TARGET_LOCATION_OK TARGET_SIZE
        env TARGET_START=1 CLAUDE_PLUGIN_ROOT="$REPO_ROOT" "$@" bash "$INIT_SCRIPT" 2>&1
    )
    return $?
}

echo "=== test-init-terminal-state-archive ==="

# --- AC1: COMPLETE state archived; fresh state written --------------------
echo ""
echo "--- AC1: COMPLETE state archived ---"
T="$TMP_BASE/ac1-complete"
make_repo "$T" "feature/x"
plant_state "$T" "COMPLETE"
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC1: init succeeds when prior state is COMPLETE"
else
    fail "AC1: expected exit 0, got $EC. Output: $OUT"
fi
if echo "$OUT" | grep -q "prior session was COMPLETE"; then
    pass "AC1: archive announcement names COMPLETE"
else
    fail "AC1: archive announcement missing. Got: $OUT"
fi
# After init, the live state file should be FRESH (status IN_PROGRESS),
# not the planted COMPLETE. Verify by reading the status line.
LIVE_STATUS=$(grep "^status:" "$T/.fno/target-state.md" | head -1)
if echo "$LIVE_STATUS" | grep -q "IN_PROGRESS"; then
    pass "AC1: live state is IN_PROGRESS (fresh init ran)"
else
    fail "AC1: live state should be IN_PROGRESS, got: $LIVE_STATUS"
fi
# Archive file should exist with the timestamped name pattern.
if ls "$T/.fno/"target-state.terminal.*.md >/dev/null 2>&1; then
    pass "AC1: archive file present"
else
    fail "AC1: no target-state.terminal.*.md archive found"
fi

# --- AC2: BLOCKED state archived ------------------------------------------
echo ""
echo "--- AC2: BLOCKED state archived ---"
T="$TMP_BASE/ac2-blocked"
make_repo "$T" "feature/x"
plant_state "$T" "BLOCKED"
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC2: init succeeds when prior state is BLOCKED"
else
    fail "AC2: expected exit 0, got $EC. Output: $OUT"
fi
if echo "$OUT" | grep -q "prior session was BLOCKED"; then
    pass "AC2: archive announcement names BLOCKED"
else
    fail "AC2: archive announcement missing. Got: $OUT"
fi
LIVE_STATUS=$(grep "^status:" "$T/.fno/target-state.md" | head -1)
if echo "$LIVE_STATUS" | grep -q "IN_PROGRESS"; then
    pass "AC2: live state is IN_PROGRESS after BLOCKED archive"
else
    fail "AC2: live state should be IN_PROGRESS, got: $LIVE_STATUS"
fi

# --- AC3: ABORTED state archived ------------------------------------------
echo ""
echo "--- AC3: ABORTED state archived ---"
T="$TMP_BASE/ac3-aborted"
make_repo "$T" "feature/x"
plant_state "$T" "ABORTED"
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC3: init succeeds when prior state is ABORTED"
else
    fail "AC3: expected exit 0, got $EC. Output: $OUT"
fi
if echo "$OUT" | grep -q "prior session was ABORTED"; then
    pass "AC3: archive announcement names ABORTED"
else
    fail "AC3: archive announcement missing. Got: $OUT"
fi

# --- AC4: IN_PROGRESS state PRESERVED (resume case, not archived) ---------
echo ""
echo "--- AC4: IN_PROGRESS state preserved on resume ---"
T="$TMP_BASE/ac4-in-progress"
make_repo "$T" "feature/x"
plant_state "$T" "IN_PROGRESS"
# Save the planted session_id so we can verify it survives.
PLANTED_SID=$(grep "^session_id:" "$T/.fno/target-state.md" | head -1)
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC4: init succeeds with IN_PROGRESS state (resume path)"
else
    fail "AC4: expected exit 0, got $EC. Output: $OUT"
fi
# The script should NOT have archived; the original session_id should be intact.
LIVE_SID=$(grep "^session_id:" "$T/.fno/target-state.md" | head -1)
if [[ "$LIVE_SID" == "$PLANTED_SID" ]]; then
    pass "AC4: original session_id preserved (no clobber)"
else
    fail "AC4: session_id changed. Planted: $PLANTED_SID, Live: $LIVE_SID"
fi
# No archive file should exist for an IN_PROGRESS preserve.
if ! ls "$T/.fno/"target-state.terminal.*.md >/dev/null 2>&1; then
    pass "AC4: no archive file (correctly preserved IN_PROGRESS)"
else
    fail "AC4: archive file created despite IN_PROGRESS preserve"
fi

# --- AC5: archive filename has timestamped format -------------------------
echo ""
echo "--- AC5: archive filename pattern ---"
T="$TMP_BASE/ac5-name"
make_repo "$T" "feature/x"
plant_state "$T" "COMPLETE"
OUT=$(run_init "$T" 2>&1)
ARCHIVE=$(ls "$T/.fno/"target-state.terminal.*.md 2>/dev/null | head -1)
if [[ -n "$ARCHIVE" ]]; then
    # Filename pattern: target-state.terminal.YYYYMMDDTHHMMSSZ.md
    if basename "$ARCHIVE" | grep -qE '^target-state\.terminal\.[0-9]{8}T[0-9]{6}Z\.md$'; then
        pass "AC5: archive filename matches expected pattern"
    else
        fail "AC5: archive filename '$(basename "$ARCHIVE")' does not match pattern"
    fi
    # Confirm planted content (status: COMPLETE) is preserved in archive.
    if grep -q "^status: COMPLETE" "$ARCHIVE"; then
        pass "AC5: archived file contains original COMPLETE status"
    else
        fail "AC5: archived file lost original status"
    fi
else
    fail "AC5: no archive file found"
fi

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]]
