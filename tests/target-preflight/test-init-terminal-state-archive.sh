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

# Plant a real-shaped immutable manifest: a transient (always-dead) owner_pid,
# NO status field, and an optional target_claim_key. This is what
# `fno target init` actually writes today. The reap keys on the CLAIM's
# liveness, not owner_pid (which is the transient init wrapper pid, dead ~1s
# after init returns - codex P1 on PR #61). Pass an empty key for the no-claim
# free-text case (must be preserved).
plant_claim_state() {
    local dir="$1"
    local key="${2:-}"
    mkdir -p "$dir/.fno"
    {
        echo "---"
        echo "session_id: planted-claim-fixture-20260626"
        echo "created_at: 2026-06-26T00:00:00Z"
        echo 'input: "x-prior"'
        echo "owner_pid: 999999"   # transient/dead: must NOT drive the reap
        echo 'owner_cwd: "/some/other/worktree"'
        [[ -n "$key" ]] && echo "target_claim_key: \"$key\""
        echo "---"
        echo ""
        echo "# Planted immutable manifest (claim-based fixture)"
    } > "$dir/.fno/target-state.md"
}

# Stub `fno` on PATH so the reaper's `fno claim status <key> --json` is
# deterministic: a key containing "live" reports state live, anything else
# reports free. No-input fresh init makes no other fno calls, so the stub need
# not pass anything through.
make_fno_stub() {
    local dir="$1"
    mkdir -p "$dir"
    cat > "$dir/fno" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "claim" && "$2" == "status" ]]; then
    case "$3" in
        *live*) printf '{"key": "%s", "state": "live"}\n' "$3" ;;
        *)      printf '{"key": "%s", "state": "free"}\n' "$3" ;;
    esac
    exit 0
fi
exit 0
STUB
    chmod +x "$dir/fno"
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
if grep -q "prior session (status COMPLETE)" <<<"$OUT"; then
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
if grep -q "prior session (status BLOCKED)" <<<"$OUT"; then
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
if grep -q "prior session (status ABORTED)" <<<"$OUT"; then
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

# --- AC6: dead (free) claim archived; transient owner_pid ignored ----------
echo ""
echo "--- AC6: non-live claim archived ---"
T="$TMP_BASE/ac6-dead-claim"
make_repo "$T" "feature/x"
STUB6="$TMP_BASE/stub6"; make_fno_stub "$STUB6"
# Claim key without "live" -> stub reports state free -> orphan -> reap.
# owner_pid is the always-dead 999999; it must NOT be what drives the reap.
plant_claim_state "$T" "node:gone-test"
OUT=$(run_init "$T" PATH="$STUB6:$PATH" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC6: init succeeds when prior claim is not live"
else
    fail "AC6: expected exit 0, got $EC. Output: $OUT"
fi
if grep -q "dead claim node:gone-test (free)" <<<"$OUT"; then
    pass "AC6: archive announcement names the dead claim"
else
    fail "AC6: dead-claim archive announcement missing. Got: $OUT"
fi
if ls "$T/.fno/"target-state.terminal.*.md >/dev/null 2>&1; then
    pass "AC6: archive file present"
else
    fail "AC6: no archive found; orphan manifest survived (the original bug)"
fi
if ! grep -q "planted-claim-fixture" "$T/.fno/target-state.md" 2>/dev/null; then
    pass "AC6: live manifest is fresh (planted orphan replaced)"
else
    fail "AC6: live manifest still the planted orphan"
fi

# --- AC7: live claim preserved (concurrent / resuming sibling) -------------
echo ""
echo "--- AC7: live claim preserved ---"
T="$TMP_BASE/ac7-live-claim"
make_repo "$T" "feature/x"
STUB7="$TMP_BASE/stub7"; make_fno_stub "$STUB7"
# Claim key contains "live" -> stub reports state live -> must be preserved,
# even though owner_pid 999999 is dead. This is the codex P1 guarantee: never
# clobber a live session's manifest off a transient owner_pid.
plant_claim_state "$T" "node:live-sess"
PLANTED_SID=$(grep "^session_id:" "$T/.fno/target-state.md" | head -1)
OUT=$(run_init "$T" PATH="$STUB7:$PATH" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC7: init succeeds with a live claim"
else
    fail "AC7: expected exit 0, got $EC. Output: $OUT"
fi
LIVE_SID=$(grep "^session_id:" "$T/.fno/target-state.md" | head -1)
if [[ "$LIVE_SID" == "$PLANTED_SID" ]]; then
    pass "AC7: live-claim manifest preserved (no clobber, dead owner_pid ignored)"
else
    fail "AC7: live-claim manifest changed. Planted: $PLANTED_SID, Live: $LIVE_SID"
fi
if ! ls "$T/.fno/"target-state.terminal.*.md >/dev/null 2>&1; then
    pass "AC7: no archive file (correctly preserved live claim)"
else
    fail "AC7: archived a live-claim manifest"
fi

# --- AC8: no claim key preserved conservatively (free-text/plan run) -------
echo ""
echo "--- AC8: no-claim manifest preserved ---"
T="$TMP_BASE/ac8-no-claim"
make_repo "$T" "feature/x"
STUB8="$TMP_BASE/stub8"; make_fno_stub "$STUB8"
# No target_claim_key at all + dead owner_pid 999999. The reaper must NOT reap
# on the transient owner_pid; a no-claim manifest is preserved.
plant_claim_state "$T" ""
PLANTED_SID=$(grep "^session_id:" "$T/.fno/target-state.md" | head -1)
OUT=$(run_init "$T" PATH="$STUB8:$PATH" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC8: init succeeds with a no-claim manifest"
else
    fail "AC8: expected exit 0, got $EC. Output: $OUT"
fi
LIVE_SID=$(grep "^session_id:" "$T/.fno/target-state.md" | head -1)
if [[ "$LIVE_SID" == "$PLANTED_SID" ]]; then
    pass "AC8: no-claim manifest preserved (transient owner_pid not used)"
else
    fail "AC8: no-claim manifest changed. Planted: $PLANTED_SID, Live: $LIVE_SID"
fi
if ! ls "$T/.fno/"target-state.terminal.*.md >/dev/null 2>&1; then
    pass "AC8: no archive file (correctly preserved no-claim manifest)"
else
    fail "AC8: archived a no-claim manifest off a transient owner_pid"
fi

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]]
