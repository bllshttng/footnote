#!/usr/bin/env bash
# tests/hooks/test_reconcile_session_start.sh
#
# Wave 1 (ab-79165ba1) of retro-auto-triage. Verifies the SessionStart
# reconcile trigger: the shared throttle helper (scripts/lib/reconcile-throttle.sh)
# fires `fno backlog reconcile` in MUTATE mode only when the throttle window has
# elapsed, and the hook (hooks/reconcile-session-start.sh) renders the prior
# sweep's result exactly once.
#
# Isolation: a FAKE `fno` is placed first on PATH so no real reconcile ever runs
# against the live graph, and render tests pin a fresh throttle stamp so the
# hook does not fire a reconcile while we assert on rendering.
#
# Run: bash tests/hooks/test_reconcile_session_start.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT_REAL="$(cd "${SCRIPT_DIR}/../.." && pwd)"
THROTTLE_LIB="${REPO_ROOT_REAL}/scripts/lib/reconcile-throttle.sh"
HOOK="${REPO_ROOT_REAL}/hooks/reconcile-session-start.sh"

log()  { printf '[reconcile-ss] %s\n' "$*"; }
fail() { printf '[reconcile-ss] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[reconcile-ss] PASS: %s\n' "$*"; }

[[ -f "$THROTTLE_LIB" ]] || fail "throttle lib not found at $THROTTLE_LIB"
[[ -f "$HOOK" ]] || fail "hook not found at $HOOK"
command -v jq >/dev/null 2>&1 || fail "jq required for these tests"

WORK=$(mktemp -d -t reconcile-ss-XXXXXX)
trap 'rm -rf "$WORK"' EXIT

# --- Fake `fno` on PATH: records its args and emits a reconcile-shaped JSON. ---
FAKEBIN="$WORK/bin"
mkdir -p "$FAKEBIN"
export FNO_CALL_LOG="$WORK/fno-calls.log"
: > "$FNO_CALL_LOG"
cat > "$FAKEBIN/fno" <<'FAKE'
#!/usr/bin/env bash
echo "$*" >> "$FNO_CALL_LOG"
if [[ "${1:-}" == "backlog" && "${2:-}" == "reconcile" ]]; then
    echo '{"dry_run": false, "candidates": [], "closed": [{"node_id":"ab-faketest","pr_number":1}], "failures": []}'
fi
# `retro run` optionally fails when the harness asks (FNO_RETRO_FAIL=1) so the
# isolation test can prove a failed harvest never aborts the chained job.
if [[ "${1:-}" == "retro" && "${2:-}" == "run" ]]; then
    [[ "${FNO_RETRO_FAIL:-0}" == "1" ]] && exit 1
    echo "(no retro-pending sentinels to triage)"
fi
FAKE
chmod +x "$FAKEBIN/fno"
export PATH="$FAKEBIN:$PATH"

# shellcheck disable=SC1090
source "$THROTTLE_LIB"

now_epoch() { date +%s; }

# Poll for a file to appear (bg reconcile is detached), up to ~4s.
wait_for_file() {
    local f="$1" tries=40
    while (( tries-- > 0 )); do
        [[ -s "$f" ]] && return 0
        sleep 0.1
    done
    return 1
}

# ============================================================================
# AC: fire when no stamp exists; MUTATE mode (no --dry-run); stamp written.
# ============================================================================
log "fire: absent stamp -> reconcile fires in mutate mode"
REPO1="$WORK/repo1"; mkdir -p "$REPO1/.fno"
RESULT1="$REPO1/.fno/.reconcile-result.json"
STAMP1="$REPO1/.fno/.reconcile-stamp"
: > "$FNO_CALL_LOG"
RECONCILE_THROTTLE_SECONDS=900 reconcile_maybe_fire "$REPO1"
[[ -f "$STAMP1" ]] || fail "fire: throttle stamp was not written"
wait_for_file "$RESULT1" || fail "fire: result json never published by bg reconcile"
grep -q "backlog reconcile --json" "$FNO_CALL_LOG" \
    || fail "fire: fno not invoked with 'backlog reconcile --json' (got: $(cat "$FNO_CALL_LOG"))"
grep -q -- "--dry-run" "$FNO_CALL_LOG" \
    && fail "fire: reconcile was invoked with --dry-run (must be mutate mode)"
grep -q "ab-faketest" "$RESULT1" || fail "fire: result json missing reconcile output"
pass "fire: absent stamp fires mutate reconcile, publishes result, writes stamp"

# AC3-HP: the same fired job chains `fno retro run` AFTER reconcile (the
# web-merge harvest backstop), so a merge that dropped no local event still gets
# its retro/carveout harvest within one throttle window.
grep -q "^retro run$" "$FNO_CALL_LOG" \
    || fail "chain: 'retro run' was not invoked in the fired job (got: $(cat "$FNO_CALL_LOG"))"
recon_line=$(grep -n "backlog reconcile --json" "$FNO_CALL_LOG" | head -1 | cut -d: -f1)
retro_line=$(grep -n "^retro run$" "$FNO_CALL_LOG" | head -1 | cut -d: -f1)
[[ -n "$recon_line" && -n "$retro_line" && "$retro_line" -gt "$recon_line" ]] \
    || fail "chain: 'retro run' did not run AFTER reconcile (recon@$recon_line retro@$retro_line)"
pass "chain: retro run fires after reconcile in the same throttled job"

# ============================================================================
# AC: gate — a directory without a .fno/ is never reconciled and is NEVER
# given a .fno/. This is the "do not litter every folder" guard: reconcile
# only ever touches a project already initialized with footnote.
# ============================================================================
log "gate: no .fno -> no fire, no .fno created"
REPO_VIRGIN="$WORK/virgin"; mkdir -p "$REPO_VIRGIN"   # deliberately NO .fno
: > "$FNO_CALL_LOG"
RECONCILE_THROTTLE_SECONDS=900 reconcile_maybe_fire "$REPO_VIRGIN"
sleep 0.3
[[ ! -e "$REPO_VIRGIN/.fno" ]] \
    || fail "gate: reconcile created a .fno in a virgin directory"
[[ ! -s "$FNO_CALL_LOG" ]] \
    || fail "gate: reconcile fired in a directory with no .fno (got: $(cat "$FNO_CALL_LOG"))"
pass "gate: virgin directory is left untouched"

# ============================================================================
# AC: throttle — a fresh stamp suppresses a second fire.
# ============================================================================
log "throttle: fresh stamp -> no second fire"
REPO2="$WORK/repo2"; mkdir -p "$REPO2/.fno"
STAMP2="$REPO2/.fno/.reconcile-stamp"
touch "$STAMP2"   # brand new stamp
: > "$FNO_CALL_LOG"
RECONCILE_THROTTLE_SECONDS=900 reconcile_maybe_fire "$REPO2"
sleep 0.3
[[ ! -s "$FNO_CALL_LOG" ]] \
    || fail "throttle: reconcile fired despite fresh stamp (got: $(cat "$FNO_CALL_LOG"))"
pass "throttle: fresh stamp within window suppresses fire"

# ============================================================================
# AC: throttle expiry — a stale stamp (older than the window) re-fires.
# ============================================================================
log "throttle: stale stamp -> re-fires"
REPO3="$WORK/repo3"; mkdir -p "$REPO3/.fno"
STAMP3="$REPO3/.fno/.reconcile-stamp"
RESULT3="$REPO3/.fno/.reconcile-result.json"
touch "$STAMP3"
: > "$FNO_CALL_LOG"
# window of 0 seconds => any existing stamp is already stale
RECONCILE_THROTTLE_SECONDS=0 reconcile_maybe_fire "$REPO3"
wait_for_file "$RESULT3" || fail "throttle: stale stamp did not re-fire reconcile"
pass "throttle: stamp older than window re-fires"

# ============================================================================
# AC: render — prior sweep with closed nodes surfaces a reminder, once.
# ============================================================================
log "render: closed nodes -> reminder emitted and result consumed"
REPO4="$WORK/repo4"; mkdir -p "$REPO4/.fno"
RESULT4="$REPO4/.fno/.reconcile-result.json"
# Pin a fresh stamp so the hook does NOT fire a reconcile during the render test.
touch "$REPO4/.fno/.reconcile-stamp"
cat > "$RESULT4" <<'JSON'
{"dry_run": false, "candidates": [], "closed": [{"node_id":"ab-aaa111","pr_number":10},{"node_id":"ab-bbb222","pr_number":11}], "failures": []}
JSON
OUT=$(CLAUDE_PROJECT_DIR="$REPO4" RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
echo "$OUT" | grep -q "closed 2 drifted node(s)" \
    || fail "render: reminder missing 'closed 2 drifted node(s)' (got: $OUT)"
echo "$OUT" | grep -q "ab-aaa111" || fail "render: reminder missing node id ab-aaa111"
echo "$OUT" | grep -q "ab-bbb222" || fail "render: reminder missing node id ab-bbb222"
[[ ! -f "$RESULT4" ]] || fail "render: result not consumed (should move to .shown)"
[[ -f "$RESULT4.shown" ]] || fail "render: consumed result not preserved as .shown"
pass "render: closed-node reminder emitted; result consumed once"

# ============================================================================
# AC: render — empty sweep is silent (no node closed => no reminder noise).
# ============================================================================
log "render: empty sweep -> silent, still consumed"
REPO5="$WORK/repo5"; mkdir -p "$REPO5/.fno"
RESULT5="$REPO5/.fno/.reconcile-result.json"
touch "$REPO5/.fno/.reconcile-stamp"
cat > "$RESULT5" <<'JSON'
{"dry_run": false, "candidates": [], "closed": [], "failures": []}
JSON
OUT=$(CLAUDE_PROJECT_DIR="$REPO5" RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
echo "$OUT" | grep -q "drifted node" \
    && fail "render: empty sweep wrongly emitted a reminder (got: $OUT)"
[[ -f "$RESULT5.shown" ]] || fail "render: empty result not consumed to .shown"
pass "render: empty sweep is silent and consumed"

# ============================================================================
# AC: non-blocking — the hook always exits 0.
# ============================================================================
log "non-blocking: hook exits 0 even with no prior result"
REPO6="$WORK/repo6"; mkdir -p "$REPO6/.fno"
touch "$REPO6/.fno/.reconcile-stamp"   # suppress fire for determinism
CLAUDE_PROJECT_DIR="$REPO6" RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" >/dev/null 2>&1 \
    || fail "non-blocking: hook returned non-zero"
pass "non-blocking: hook exits 0 with no prior result"

# ============================================================================
# AC3-ERR: a failing `retro run` (|| true) never aborts the chained job -
# reconcile result still publishes and tidy still runs. (Sentinel RETENTION on
# failure is retro run's own contract; here we prove the chain's isolation.)
# ============================================================================
log "chain: retro run failure does not sink the job"
REPO_RF="$WORK/repo-retrofail"; mkdir -p "$REPO_RF/.fno"
RESULT_RF="$REPO_RF/.fno/.reconcile-result.json"
: > "$FNO_CALL_LOG"
FNO_RETRO_FAIL=1 RECONCILE_THROTTLE_SECONDS=0 reconcile_maybe_fire "$REPO_RF"
wait_for_file "$RESULT_RF" || fail "chain-fail: result json not published despite retro failing"
grep -q "^retro run$" "$FNO_CALL_LOG" || fail "chain-fail: retro run not attempted"
grep -q "backlog capture tidy" "$FNO_CALL_LOG" \
    || fail "chain-fail: tidy skipped after retro failure (job aborted early)"
pass "chain: failed retro run is isolated; reconcile publish + tidy still run"

# ============================================================================
# AC3-UI: >=1 pending sentinel -> advisory line with the count renders.
# AC3-EDGE: 0 pending sentinels -> silent (no advisory line).
# The pending dir is env-injected (RETRO_PENDING_DIR) so the test never touches
# the real ~/.fno; a fresh stamp suppresses a fire during the render assertion.
# ============================================================================
log "advisory: pending sentinels render a count line"
REPO_ADV="$WORK/repo-adv"; mkdir -p "$REPO_ADV/.fno"
touch "$REPO_ADV/.fno/.reconcile-stamp"
PENDING_DIR="$WORK/pending-adv"; mkdir -p "$PENDING_DIR"
printf '{"node_id":"x-1111","pr_url":"https://github.com/o/r/pull/1"}' > "$PENDING_DIR/x-1111.json"
printf '{"node_id":"x-2222","pr_url":"https://github.com/o/r/pull/2"}' > "$PENDING_DIR/x-2222.json"
OUT=$(CLAUDE_PROJECT_DIR="$REPO_ADV" RETRO_PENDING_DIR="$PENDING_DIR" \
      RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
echo "$OUT" | grep -q "retro: 2 sentinel(s) pending harvest" \
    || fail "advisory: missing '2 sentinel(s) pending harvest' line (got: $OUT)"
pass "advisory: pending count line renders"

log "advisory: zero sentinels is silent"
REPO_ADV0="$WORK/repo-adv0"; mkdir -p "$REPO_ADV0/.fno"
touch "$REPO_ADV0/.fno/.reconcile-stamp"
EMPTY_DIR="$WORK/pending-empty"; mkdir -p "$EMPTY_DIR"
OUT=$(CLAUDE_PROJECT_DIR="$REPO_ADV0" RETRO_PENDING_DIR="$EMPTY_DIR" \
      RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
echo "$OUT" | grep -q "pending harvest" \
    && fail "advisory: emitted a line with zero pending sentinels (got: $OUT)"
pass "advisory: zero sentinels is silent"

echo "[reconcile-ss] all reconcile-session-start tests passed"
exit 0
