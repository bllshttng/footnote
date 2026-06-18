#!/usr/bin/env bash
# test_pending_plan_wipe.sh - verify init-target-state.sh's session-start wipe
# of a stale .fno/.pending-plan.md sidecar (task 2.2).
#
# Covers:
#   AC1-EDGE: stale sidecar (past TTL, or prior session_id) is wiped at init.
#   Concurrency: a fresh same-session sidecar survives so /target can detect it.
#
# Mirrors tests/hooks/test_init_target_state_mission_fields.sh: a temp git repo
# with NO commit (unborn HEAD bypasses the location gate), TARGET_START=1.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INIT="${REPO_ROOT}/hooks/helpers/init-target-state.sh"

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }
skip() { echo "SKIP: $1" >&2; exit 0; }

command -v git     >/dev/null 2>&1 || skip "git not on PATH"
command -v python3 >/dev/null 2>&1 || skip "python3 not on PATH"
[[ -f "$INIT" ]] || { echo "init script missing: $INIT" >&2; exit 1; }

bash -n "$INIT" || { echo "bash -n rejected $INIT" >&2; exit 1; }

# Write a sidecar with a given session_id into <repo>/.fno/.pending-plan.md
write_sidecar() {
  local dir="$1" sid="$2"
  mkdir -p "$dir/.fno"
  cat > "$dir/.fno/.pending-plan.md" <<EOF
---
captured_at: 2026-06-02T21:30:00Z
session_id: $sid
slug: add-csv-export
source: claude-plan-mode
status: pending
---

# Add CSV export

native plan body
EOF
}

# Run a fresh init in a temp repo. Args: claude_sid ttl
run_init() {
  local dir="$1" claude_sid="$2" ttl="${3:-14400}"
  ( cd "$dir" \
    && TARGET_START=1 TARGET_INPUT="test" \
       TARGET_TRANSCRIPT_ID="$claude_sid" \
       PENDING_PLAN_TTL_SECONDS="$ttl" \
       bash "$INIT" >/dev/null 2>&1 )
}

# --- Case 1: fresh same-session sidecar SURVIVES ---
T1=$(mktemp -d -t pp-wipe-survive.XXXXXX)
trap 'rm -rf "$T1" "${T2:-}" "${T3:-}" "${T4:-}"' EXIT
( cd "$T1" && git init -q )
write_sidecar "$T1" "sess-SAME-123"
run_init "$T1" "sess-SAME-123" 14400
[[ -f "$T1/.fno/.pending-plan.md" ]] \
  && pass "survive: same-session fresh sidecar kept" \
  || fail "survive: same-session sidecar was wiped (should survive)"

# --- Case 2: prior-session sidecar is WIPED ---
T2=$(mktemp -d -t pp-wipe-session.XXXXXX)
( cd "$T2" && git init -q )
write_sidecar "$T2" "sess-OTHER-999"
run_init "$T2" "sess-CURRENT-123" 14400
[[ ! -f "$T2/.fno/.pending-plan.md" ]] \
  && pass "session-mismatch: prior-session sidecar wiped" \
  || fail "session-mismatch: stale sidecar survived (should be wiped)"

# --- Case 3: past-TTL sidecar is WIPED (even same session) ---
T3=$(mktemp -d -t pp-wipe-ttl.XXXXXX)
( cd "$T3" && git init -q )
write_sidecar "$T3" "sess-SAME-123"
# Age the sidecar past a tiny TTL by back-dating its mtime ~1h.
if touch -d '1 hour ago' "$T3/.fno/.pending-plan.md" 2>/dev/null; then :; \
elif touch -A -010000 "$T3/.fno/.pending-plan.md" 2>/dev/null; then :; \
else touch -t "$(date -u -v-1H '+%Y%m%d%H%M' 2>/dev/null || echo 202601010000)" "$T3/.fno/.pending-plan.md" 2>/dev/null; fi
run_init "$T3" "sess-SAME-123" 60   # TTL 60s; sidecar is ~1h old
[[ ! -f "$T3/.fno/.pending-plan.md" ]] \
  && pass "ttl: past-TTL sidecar wiped even with matching session" \
  || fail "ttl: stale (past-TTL) sidecar survived (should be wiped)"

# --- Case 4: no Claude session id available -> TTL alone, fresh survives ---
T4=$(mktemp -d -t pp-wipe-nosid.XXXXXX)
( cd "$T4" && git init -q )
write_sidecar "$T4" "sess-whatever"
( cd "$T4" \
  && unset TARGET_TRANSCRIPT_ID CLAUDE_CODE_SESSION_ID \
  && TARGET_START=1 TARGET_INPUT="test" PENDING_PLAN_TTL_SECONDS="14400" \
     bash "$INIT" >/dev/null 2>&1 )   # session id genuinely unavailable -> TTL only
[[ -f "$T4/.fno/.pending-plan.md" ]] \
  && pass "no-sid: fresh sidecar survives when session id unavailable (TTL only)" \
  || fail "no-sid: fresh sidecar wiped despite no session id and in-TTL"

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
