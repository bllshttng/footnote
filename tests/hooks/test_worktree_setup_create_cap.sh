#!/usr/bin/env bash
# test_worktree_setup_create_cap.sh
#
# The session-keyed create-attempt cap in hooks/worktree-setup.sh: repeated
# worktree ceremony in one session must abort loudly at the cap (exit 0 with
# EMPTY stdout - non-zero falls back to CC's default flow and creates the
# worktree being refused), a successful create clears the latch, and a
# payload with no session_id (manual callers) is never counted.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/worktree-setup.sh"

if [[ ! -f "$HOOK" ]]; then
    echo "FAIL: hook not found at $HOOK" >&2
    exit 1
fi

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP_BASE="$(mktemp -d -t wt-create-cap-XXXXXX)"
trap 'rm -rf "$TMP_BASE"' EXIT

make_repo() {
    local dir="$1"
    mkdir -p "$dir"
    (
        cd "$dir"
        git init -q -b main
        git config user.email t@t.com
        git config user.name Test
        echo "# x" > README.md
        git add README.md
        git commit -q -m init
    )
}

# Hermetic fno: config carries a tmp state_dir (so the latch never lands in
# the real ~/.fno) and harness-native policy (no relocation surprises).
export HOME="$TMP_BASE/home"
mkdir -p "$HOME"
export FNO_CONFIG="$TMP_BASE/config.toml"
cat > "$FNO_CONFIG" <<EOF
state_dir = "$TMP_BASE/state"

[worktree]
policy = "harness-native"
EOF

run_hook() { # <cwd> <json-payload>; stdout captured by the caller
    ( cd "$1" && printf '%s' "$2" | bash "$HOOK" 2>"$TMP_BASE/stderr.txt" )
}

echo "=== test_worktree_setup_create_cap ==="

# --- AC4-HP: three attempts pass, the fourth aborts exit 0 + empty stdout ---
echo ""
echo "--- cap fires on the fourth create attempt ---"
CANON="$TMP_BASE/canon"; make_repo "$CANON"
DEFER_PAYLOAD='{"session_id":"capsess-1234","hook_event_name":"WorktreeCreate","name":"w1"}'
for i in 1 2 3; do
    OUT="$(run_hook "$CANON" "$DEFER_PAYLOAD")"
    RC=$?
    if [[ $RC -ne 0 && -z "$OUT" ]]; then
        pass "attempt $i still defers (exit $RC, empty stdout)"
    else
        fail "attempt $i: expected defer (non-zero, empty stdout), got exit $RC out='$OUT'"
    fi
done
OUT="$(run_hook "$CANON" "$DEFER_PAYLOAD")"
RC=$?
[[ $RC -eq 0 ]] && pass "attempt 4 exits 0 (the supported abort)" || fail "attempt 4 exit was $RC"
[[ -z "$OUT" ]] && pass "attempt 4 stdout is empty" || fail "attempt 4 stdout='$OUT'"
if grep -q "capping worktree ceremony at 3" "$TMP_BASE/stderr.txt" \
    && grep -q "capsess-1234" "$TMP_BASE/stderr.txt" \
    && grep -q "FNO_WORKTREE_POLICY=never" "$TMP_BASE/stderr.txt"; then
    pass "refusal names the session, the cap, and the override"
else
    fail "refusal text incomplete: $(cat "$TMP_BASE/stderr.txt")"
fi

# --- AC4-EDGE: a successful create clears the latch -------------------------
echo ""
echo "--- successful create clears the latch ---"
CANON2="$TMP_BASE/canon2"; make_repo "$CANON2"
WT2="$CANON2/.claude/worktrees/w9"
( cd "$CANON2" && git worktree add -q "$WT2" -b wt-w9 )
OK_PAYLOAD="{\"session_id\":\"capsess-done\",\"hook_event_name\":\"WorktreeCreate\",\"name\":\"w9\",\"path\":\"$WT2\"}"
OUT="$(run_hook "$CANON2" "$OK_PAYLOAD")"
RC=$?
[[ $RC -eq 0 ]] && pass "create with a pre-made worktree exits 0" || fail "create exit was $RC"
[[ "$OUT" == "$WT2" ]] && pass "stdout carries the worktree path" || fail "stdout was '$OUT'"
LATCH_FOUND="$(find "$TMP_BASE/state/latches" "$HOME/.fno/latches" -maxdepth 1 -name '.worktree-create-capsess-done' 2>/dev/null)"
[[ -z "$LATCH_FOUND" ]] && pass "latch deleted by its writer" || fail "latch survived: $LATCH_FOUND"

# --- a payload with no session_id is never counted ---------------------------
echo ""
echo "--- no session_id: uncapped (manual callers) ---"
CANON3="$TMP_BASE/canon3"; make_repo "$CANON3"
NOSESSION='{"hook_event_name":"WorktreeCreate","name":"w1"}'
CAPPED=0
for i in 1 2 3 4 5; do
    OUT="$(run_hook "$CANON3" "$NOSESSION")"
    RC=$?
    if [[ $RC -eq 0 && -z "$OUT" ]]; then
        CAPPED=$((CAPPED + 1))
    fi
done
[[ $CAPPED -eq 0 ]] && pass "five uncapped attempts, none aborted by the cap" || fail "$CAPPED attempt(s) were capped without a session_id"

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]]
