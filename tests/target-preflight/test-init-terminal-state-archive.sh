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

# Plant a real-shaped immutable manifest: owner_pid, NO status field. This is
# what `fno target init` actually writes today; liveness of owner_pid is the
# only orphan signal (the status-string reaper never fires for these).
plant_pid_state() {
    local dir="$1"
    local pid="$2"
    mkdir -p "$dir/.fno"
    cat > "$dir/.fno/target-state.md" <<EOF
---
session_id: planted-pid-fixture-20260626
created_at: 2026-06-26T00:00:00Z
input: "x-prior"
owner_pid: $pid
owner_cwd: "/some/other/worktree"
---

# Planted immutable manifest (owner_pid, no status)
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
if echo "$OUT" | grep -q "prior session (status COMPLETE)"; then
    pass "AC1: archive announcement names COMPLETE"
else
    fail "AC1: archive announcement missing. Got: $OUT"
fi
# After init, the live state file should be FRESH (status IN_PROGRESS),
# not the planted COMPLETE. Verify by reading the status line.
# Fresh init must have run: the immutable manifest carries no status field, so
# verify freshness by the planted orphan content being gone, not a status line.
if ! grep -q "planted-fixture-20260522" "$T/.fno/target-state.md" 2>/dev/null; then
    pass "AC1: live manifest is fresh (planted orphan replaced)"
else
    fail "AC1: live manifest still the planted orphan (fresh init did not run)"
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
if echo "$OUT" | grep -q "prior session (status BLOCKED)"; then
    pass "AC2: archive announcement names BLOCKED"
else
    fail "AC2: archive announcement missing. Got: $OUT"
fi
if ! grep -q "planted-fixture-20260522" "$T/.fno/target-state.md" 2>/dev/null; then
    pass "AC2: live manifest is fresh after BLOCKED archive"
else
    fail "AC2: live manifest still the planted orphan after BLOCKED archive"
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
if echo "$OUT" | grep -q "prior session (status ABORTED)"; then
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

# --- AC6: dead owner_pid archived (real immutable manifest, no status) -----
echo ""
echo "--- AC6: dead owner_pid archived ---"
T="$TMP_BASE/ac6-dead-pid"
make_repo "$T" "feature/x"
# PID 999999 is well above typical pid_max and reliably dead.
plant_pid_state "$T" "999999"
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC6: init succeeds when prior owner_pid is dead"
else
    fail "AC6: expected exit 0, got $EC. Output: $OUT"
fi
if echo "$OUT" | grep -q "dead owner_pid 999999"; then
    pass "AC6: archive announcement names the dead owner_pid"
else
    fail "AC6: dead-pid archive announcement missing. Got: $OUT"
fi
if ls "$T/.fno/"target-state.terminal.*.md >/dev/null 2>&1; then
    pass "AC6: archive file present"
else
    fail "AC6: no archive found; orphan manifest survived (the original bug)"
fi
# Fresh init must have run: the planted session_id must be gone from the live file.
if ! grep -q "planted-pid-fixture" "$T/.fno/target-state.md" 2>/dev/null; then
    pass "AC6: live manifest is fresh (planted orphan replaced)"
else
    fail "AC6: live manifest still the planted orphan"
fi

# --- AC7: live owner_pid preserved (resume / concurrent sibling) -----------
echo ""
echo "--- AC7: live owner_pid preserved ---"
T="$TMP_BASE/ac7-live-pid"
make_repo "$T" "feature/x"
# $$ is this test process: guaranteed alive for the duration of run_init.
plant_pid_state "$T" "$$"
PLANTED_SID=$(grep "^session_id:" "$T/.fno/target-state.md" | head -1)
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC7: init succeeds with a live owner_pid"
else
    fail "AC7: expected exit 0, got $EC. Output: $OUT"
fi
LIVE_SID=$(grep "^session_id:" "$T/.fno/target-state.md" | head -1)
if [[ "$LIVE_SID" == "$PLANTED_SID" ]]; then
    pass "AC7: live-owner manifest preserved (no clobber)"
else
    fail "AC7: live-owner manifest changed. Planted: $PLANTED_SID, Live: $LIVE_SID"
fi
if ! ls "$T/.fno/"target-state.terminal.*.md >/dev/null 2>&1; then
    pass "AC7: no archive file (correctly preserved live owner)"
else
    fail "AC7: archived a live-owner manifest"
fi

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]]
