#!/usr/bin/env bash
# test_detect_pending_plan.sh - verify skills/target/scripts/detect-pending-plan.sh
# detection, precedence, body extraction, and atomic consume (tasks 4.1 + 4.2).
#
# Covers:
#   AC1-HP   bare /target + fresh pending sidecar -> result=pending + slug + age
#   AC1-FR   declined confirm (no consume) leaves the sidecar pending/re-offerable;
#            consume is idempotent (already-consumed -> exit 3, not re-consumed)
#   AC1-EDGE / AC3-EDGE  expired (past-TTL) sidecar -> treated as absent
#   AC3-HP   explicit arg + fresh sidecar -> result=superseded_by_arg (sidecar stays pending)
#   AC3-ERR  malformed sidecar -> result=malformed, logged, never fatal
#   AC4-EDGE no sidecar -> result=none
#   plus: body extraction is verbatim; consumed sidecar is inert (result=none).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DP="$REPO_ROOT/skills/target/scripts/detect-pending-plan.sh"
TMP=$(mktemp -d -t detect-pending.XXXXXX)
# Isolate claim state so consume's `fno claim` never touches real ~/.fno.
export FNO_CLAIMS_ROOT="$TMP/claims-root"
mkdir -p "$FNO_CLAIMS_ROOT"
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

[[ -f "$DP" ]] || { echo "detect script missing: $DP" >&2; exit 1; }
bash -n "$DP" || { echo "bash -n rejected $DP" >&2; exit 1; }

SC="$TMP/.pending-plan.md"
EVENTS="$TMP/hook-events.jsonl"

write_sidecar() {  # status [source] [captured_at]
  local status="${1:-pending}" source="${2:-claude-plan-mode}" captured="${3:-2026-06-02T21:30:00Z}"
  cat > "$SC" <<EOF
---
captured_at: $captured
session_id: sess-1
slug: add-csv-export
source: $source
status: $status
---

# Add CSV export

Body line with special: \$HOME \`code\` & < >
EOF
}
# detect() and consume() log to dirname(sidecar)/hook-events.jsonl -> $TMP.

# --- AC4-EDGE: no sidecar -> none ---
rm -f "$SC"
OUT="$(bash "$DP" detect --sidecar "$SC")"
echo "$OUT" | grep -q '^result=none$' && pass "no sidecar -> result=none" || fail "no sidecar: $OUT"

# --- AC1-HP: fresh pending, no arg -> pending + slug + age ---
write_sidecar pending
OUT="$(bash "$DP" detect --sidecar "$SC")"
echo "$OUT" | grep -q '^result=pending$' && pass "fresh pending -> result=pending" || fail "pending: $OUT"
echo "$OUT" | grep -q '^slug=add-csv-export$' && pass "pending: slug reported" || fail "pending: slug missing"
echo "$OUT" | grep -qE '^age_human=[0-9]+[smh]$' && pass "pending: age_human reported" || fail "pending: age_human missing"

# --- AC3-HP: explicit arg wins (sidecar stays pending) ---
OUT="$(bash "$DP" detect --sidecar "$SC" --arg "a different feature")"
echo "$OUT" | grep -q '^result=superseded_by_arg$' && pass "explicit arg -> superseded_by_arg" || fail "arg precedence: $OUT"
grep -q '^status: pending$' "$SC" && pass "explicit arg: sidecar stays pending (not consumed)" || fail "arg precedence: sidecar mutated"

# --- AC3-ERR: malformed sidecar (wrong source) -> malformed, logged ---
rm -f "$EVENTS"
write_sidecar pending "some-other-source"
OUT="$(bash "$DP" detect --sidecar "$SC")"
echo "$OUT" | grep -q '^result=malformed$' && pass "wrong source -> result=malformed" || fail "malformed: $OUT"
grep -q 'plan_mode_sidecar_malformed' "$EVENTS" 2>/dev/null && pass "malformed: logged to hook-events" || fail "malformed: not logged"
# malformed + explicit arg still yields malformed (arg runs cleanly, never blocked)
OUT="$(bash "$DP" detect --sidecar "$SC" --arg "x")"
echo "$OUT" | grep -q '^result=malformed$' && pass "malformed + arg -> malformed (arg unblocked)" || fail "malformed+arg: $OUT"

# --- AC3-EDGE / AC1-EDGE: expired (past TTL) -> treated absent ---
write_sidecar pending
# Back-date mtime ~2h; TTL 60s. GNU `touch -d` first; BSD `touch -A` is a
# tz-safe RELATIVE adjust (avoids the UTC-vs-local trap of `touch -t`).
touch -d '2 hours ago' "$SC" 2>/dev/null || touch -A -020000 "$SC" 2>/dev/null || true
OUT="$(PENDING_PLAN_TTL_SECONDS=60 bash "$DP" detect --sidecar "$SC")"
echo "$OUT" | grep -q '^result=expired$' && pass "past-TTL -> result=expired (absent)" || fail "expired: $OUT"

# --- body: verbatim extraction (frontmatter stripped) ---
write_sidecar pending
bash "$DP" body "$TMP/body.md" --sidecar "$SC"
if grep -qF '# Add CSV export' "$TMP/body.md" \
   && grep -qF 'Body line with special: $HOME `code` & < >' "$TMP/body.md" \
   && ! grep -q '^source: claude-plan-mode$' "$TMP/body.md"; then
  pass "body: native body extracted verbatim, frontmatter stripped"
else
  fail "body: extraction wrong"; cat "$TMP/body.md" >&2
fi

# --- consume: pending -> consumed (exit 0), then inert ---
write_sidecar pending
if bash "$DP" consume --sidecar "$SC" --holder "test-A" >/dev/null 2>&1; then
  pass "consume: first consume exits 0"
else
  fail "consume: first consume failed"
fi
grep -q '^status: consumed$' "$SC" && pass "consume: status flipped to consumed" || fail "consume: status not flipped"
# A consumed sidecar is inert to detect.
OUT="$(bash "$DP" detect --sidecar "$SC")"
echo "$OUT" | grep -q '^result=none$' && pass "consumed sidecar -> detect result=none" || fail "consumed: $OUT"

# --- consume idempotency: second consume must NOT re-consume (exit 3) ---
if bash "$DP" consume --sidecar "$SC" --holder "test-B" >/dev/null 2>&1; then
  fail "consume: second consume should have exited 3"
else
  rc=$?; [[ $rc -eq 3 ]] && pass "consume: already-consumed exits 3 (no double-run)" || fail "consume: second exit $rc (expected 3)"
fi

# --- AC1-FR: declined confirm leaves sidecar re-offerable (never consumed) ---
write_sidecar pending
# Simulate decline: detect (pending) but do NOT call consume.
bash "$DP" detect --sidecar "$SC" >/dev/null
grep -q '^status: pending$' "$SC" && pass "declined: sidecar still pending (re-offerable)" || fail "declined: sidecar changed without consume"

# --- review fix: body extraction on a TORN sidecar errors, never empty-and-ok ---
TORN="$TMP/torn.md"
printf -- '---\ncaptured_at: 2026-06-02T21:30:00Z\nsession_id: s\nslug: x\nsource: claude-plan-mode\nstatus: pending\n' > "$TORN"  # NOTE: no closing '---'
if bash "$DP" body "$TMP/torn-body.md" --sidecar "$TORN" >/dev/null 2>&1; then
  fail "body: torn sidecar (no closing ---) should error, not exit 0"
else
  rc=$?; [[ $rc -eq 2 ]] && pass "body: torn sidecar -> exit 2 (not silent empty)" || fail "body: torn exit $rc (expected 2)"
fi
[[ ! -s "$TMP/torn-body.md" ]] && pass "body: no empty body file left behind" || fail "body: empty body file written"

# --- review fix: consume releases the claim on success (same-slug re-approval not blocked) ---
write_sidecar pending
bash "$DP" consume --sidecar "$SC" --holder "target-session:run-1" >/dev/null 2>&1
# Re-approval: capture hook would overwrite the sidecar back to pending (same slug).
write_sidecar pending
if bash "$DP" consume --sidecar "$SC" --holder "target-session:run-2" >/dev/null 2>&1; then
  pass "consume: same-slug re-approval consumes (claim released on prior success)"
else
  rc=$?; fail "consume: re-approval falsely blocked (exit $rc) - claim not released"
fi

# --- review fix: consume cleans up its mkdir lock (no leak) ---
write_sidecar pending
bash "$DP" consume --sidecar "$SC" --holder "target-session:run-3" >/dev/null 2>&1
[[ ! -e "$SC.consume.lock" ]] && pass "consume: mkdir lock removed (trap cleanup)" || fail "consume: lock dir leaked"

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
