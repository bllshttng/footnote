#!/usr/bin/env bash
# test_target_guard_owner_pid.sh - x-6044: target_is_active() reads owner_pid
# liveness, not a `status:` field the manifest writer no longer emits. Before
# this fix the dead `status == "IN_PROGRESS"` gate returned "not active" for
# every current-format manifest, silently disabling every consumer (session
# reminder, subagent guard, cache keepalive, attest-model).
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
GUARD="$REPO_ROOT/scripts/lib/target-guard.sh"

# shellcheck source=/dev/null
source "$GUARD"

pass=0
fail=0
check() {
    local desc="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        echo "PASS: $desc"; pass=$((pass + 1))
    else
        echo "FAIL: $desc (expected rc=$expected actual rc=$actual)"; fail=$((fail + 1))
    fi
}

# Write a manifest into a fresh tmp dir and return its path.
manifest() {
    local dir; dir="$(mktemp -d)"
    printf '%s\n' "$@" > "$dir/target-state.md"
    echo "$dir/target-state.md"
}

# A guaranteed-dead pid: spawn `true`, reap it, reuse its (now-freed) pid.
dead_pid() { local p; ( : ) & p=$!; wait "$p" 2>/dev/null; echo "$p"; }

# --- BDD: current-format live manifest (owner_pid = this shell, no status) ---
f="$(manifest "input: x-7a6d" "plan_path: /tmp/plan.md" "owner_pid: $$")"
target_is_active "$f"; check "current-format live manifest -> active" 0 $?

# --- BDD: manifest whose owner_pid is dead -> not active ---
DEAD="$(dead_pid)"
f="$(manifest "input: x-7a6d" "owner_pid: $DEAD")"
target_is_active "$f"; check "dead owner_pid -> not active" 1 $?

# --- BDD: legacy manifest carrying status: IN_PROGRESS + live owner_pid ---
# The dropped gate must never demote a live session (back-compat).
f="$(manifest "status: IN_PROGRESS" "input: x-7a6d" "owner_pid: $$")"
target_is_active "$f"; check "legacy status:IN_PROGRESS + live pid -> active" 0 $?

# --- Invariant: absent owner_pid stays active (pre-owner-pid legacy, fail open) ---
f="$(manifest "input: x-7a6d" "plan_path: /tmp/plan.md")"
target_is_active "$f"; check "absent owner_pid -> active (fail open)" 0 $?

# --- Empty-input stub is never active ---
f="$(manifest "input:" "plan_path:")"
target_is_active "$f"; check "empty-input stub -> not active" 1 $?

# --- Missing file is never active ---
target_is_active "/nonexistent/target-state.md"; check "missing file -> not active" 1 $?

echo "----"
echo "passed=$pass failed=$fail"
[ "$fail" -eq 0 ]
