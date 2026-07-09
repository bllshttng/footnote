#!/usr/bin/env bash
# test_target_guard_claim_liveness.sh - x-6044: target_is_active() reads the node
# CLAIM's liveness (via `fno claim status`), not the manifest `status:` field
# (the writer no longer emits it) and NOT owner_pid (the transient init-wrapper
# pid, dead ~1s after init returns, which reads a live session as inactive).
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

# A sandbox with a stubbed `fno` whose `claim status <key> -J` reports $STATE.
# Returns the manifest path; PATH is rewired for the duration of each call.
STUB_STATE="live"
make_fno_stub() {
    local dir; dir="$(mktemp -d)"
    mkdir -p "$dir/bin"
    cat > "$dir/bin/fno" <<STUB
#!/usr/bin/env bash
if [ "\$1" = "claim" ] && [ "\$2" = "status" ]; then
  printf '{"key": "%s", "state": "%s", "pid": 111}\n' "\$3" "${STUB_STATE}"
  exit 0
fi
exit 0
STUB
    chmod +x "$dir/bin/fno"
    echo "$dir"
}

manifest() {
    local dir; dir="$(mktemp -d)"
    printf '%s\n' "$@" > "$dir/target-state.md"
    echo "$dir/target-state.md"
}

# --- BDD: current-format live manifest (claim live) -> active ---
STUB_STATE="live"; STUB="$(make_fno_stub)"
f="$(manifest "input: x-7a6d" 'target_claim_key: "node:x-7a6d"')"
PATH="$STUB/bin:$PATH" target_is_active "$f"; check "live claim -> active" 0 $?

# --- BDD: claim STALE (dead durable holder) -> not active ---
STUB_STATE="stale"; STUB="$(make_fno_stub)"
f="$(manifest "input: x-7a6d" 'target_claim_key: "node:x-7a6d"')"
PATH="$STUB/bin:$PATH" target_is_active "$f"; check "stale claim -> not active" 1 $?

# --- BDD: claim SUSPECT (TTL-protected respawn) -> active ---
STUB_STATE="suspect"; STUB="$(make_fno_stub)"
f="$(manifest "input: x-7a6d" 'target_claim_key: "node:x-7a6d"')"
PATH="$STUB/bin:$PATH" target_is_active "$f"; check "suspect claim -> active" 0 $?

# --- Invariant: owner_pid is NOT consulted (a dead owner_pid with a live claim
#     must still read active — the whole point of x-6044) ---
STUB_STATE="live"; STUB="$(make_fno_stub)"
DEAD=999999; while kill -0 "$DEAD" 2>/dev/null; do DEAD=$((DEAD+1)); done
f="$(manifest "input: x-7a6d" "owner_pid: $DEAD" 'target_claim_key: "node:x-7a6d"')"
PATH="$STUB/bin:$PATH" target_is_active "$f"; check "dead owner_pid + live claim -> active" 0 $?

# --- Fail open: no claim key on the manifest (free-text / pre-claim legacy) ---
f="$(manifest "input: x-7a6d" "plan_path: /tmp/p.md")"
target_is_active "$f"; check "no claim key -> active (fail open)" 0 $?

# --- Fail open: fno unavailable -> active (claims subsystem unreadable) ---
f="$(manifest "input: x-7a6d" 'target_claim_key: "node:x-7a6d"')"
# Minimal PATH with no `fno` on it.
PATH="/usr/bin:/bin" target_is_active "$f"; check "fno unavailable -> active (fail open)" 0 $?

# --- Empty-input stub is never active ---
f="$(manifest "input:" "plan_path:")"
target_is_active "$f"; check "empty-input stub -> not active" 1 $?

# --- Missing file is never active ---
target_is_active "/nonexistent/target-state.md"; check "missing file -> not active" 1 $?

# Fail-open survives a caller's `set -euo pipefail` even as a BARE statement
# (not a condition): target_state_field's grep returns non-zero on an absent
# field under pipefail, and `|| true` keeps the assignment from aborting.
(
  set -euo pipefail
  source "$GUARD"
  f2="$(mktemp -d)/target-state.md"
  printf 'input: x-7a6d\nplan_path: /tmp/p.md\n' > "$f2"
  target_is_active "$f2"
) && rc=$? || rc=$?
check "fail-open under set -euo pipefail (bare call, no claim key)" 0 "$rc"

echo "----"
echo "passed=$pass failed=$fail"
[ "$fail" -eq 0 ]
