#!/usr/bin/env bash
# tests/hooks/test_reconcile_session_start.sh
#
# Wave 1 (ab-79165ba1) of retro-auto-triage. Verifies the SessionStart
# reconcile trigger: the shared throttle helper (scripts/lib/reconcile-throttle.sh)
# fires `fno backlog reconcile` in MUTATE mode only when the throttle window has
# elapsed. The hook no longer renders the sweep's result: the notice_route
# daemon arm routes those warnings to the owning lead and consumes the result
# files itself, so these tests pin the hook's silence and its leaving every
# result file intact for that arm.
#
# Isolation: a FAKE `fno` is placed first on PATH so no real reconcile ever runs
# against the live graph, and silence tests pin a fresh throttle stamp so the
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
git -C "$WORK" init -q
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

# Poll for the last command in a detached reconcile chain. The result file is
# published before retro/tidy run, so its presence cannot prove the job finished.
wait_for_log_line() {
    local pattern="$1" tries=40
    while (( tries-- > 0 )); do
        grep -q "$pattern" "$FNO_CALL_LOG" 2>/dev/null && return 0
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
wait_for_log_line "^retro drain-postmortems$" \
    || fail "fire: detached reconcile chain never completed"
grep -q "backlog reconcile --json" "$FNO_CALL_LOG" \
    || fail "fire: fno not invoked with 'backlog reconcile --json' (got: $(cat "$FNO_CALL_LOG"))"
grep -q -- "--dry-run" "$FNO_CALL_LOG" \
    && fail "fire: reconcile was invoked with --dry-run (must be mutate mode)"
grep -q "ab-faketest" "$RESULT1" || fail "fire: result json missing reconcile output"
pass "fire: absent stamp fires mutate reconcile, publishes result, writes stamp"

# AC3-HP: the same fired job chains `fno backlog retro run` AFTER reconcile (the
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
# AC: silence — prior sweep with closed nodes is NOT the hook's business:
# the notice_route daemon arm routes them to the owning lead. The hook
# stays silent and leaves the result file for that arm to consume.
# ============================================================================
log "render: closed nodes -> hook silent, result left for the notice arm"
REPO4="$WORK/repo4"; mkdir -p "$REPO4/.fno"
git -C "$REPO4" init -q
RESULT4="$REPO4/.fno/.reconcile-result.json"
# Pin a fresh stamp so the hook does NOT fire a reconcile during the render test.
touch "$REPO4/.fno/.reconcile-stamp"
cat > "$RESULT4" <<'JSON'
{"dry_run": false, "candidates": [], "closed": [{"node_id":"ab-aaa111","pr_number":10},{"node_id":"ab-bbb222","pr_number":11}], "failures": []}
JSON
OUT=$(CLAUDE_PROJECT_DIR="$REPO4" RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
grep -q "drifted node" <<<"$OUT" \
    && fail "render: hook still emits the worker-scope reminder (got: $OUT)"
grep -q "ab-aaa111" <<<"$OUT" \
    && fail "render: hook still surfaces node ids to every session (got: $OUT)"
grep -q "ab-aaa111" <"$RESULT4" \
    || fail "render: hook disturbed the result the notice arm must read"
[[ -f "$RESULT4.shown" ]] \
    && fail "render: the hook consumed a result the notice arm owns"
pass "render: hook silent on closed nodes; result intact for the notice arm"

# ============================================================================
# AC: an empty sweep is equally silent, and equally untouched.
# ============================================================================
log "render: empty sweep -> silent, untouched"
REPO5="$WORK/repo5"; mkdir -p "$REPO5/.fno"
git -C "$REPO5" init -q
RESULT5="$REPO5/.fno/.reconcile-result.json"
touch "$REPO5/.fno/.reconcile-stamp"
cat > "$RESULT5" <<'JSON'
{"dry_run": false, "candidates": [], "closed": [], "failures": []}
JSON
OUT=$(CLAUDE_PROJECT_DIR="$REPO5" RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
grep -q "drifted node" <<<"$OUT" \
    && fail "render: empty sweep wrongly emitted a reminder (got: $OUT)"
[[ -f "$RESULT5" ]] || fail "render: hook consumed the empty result the notice arm owns"
pass "render: empty sweep is silent and untouched"

# ============================================================================
# AC: promise-gate held-open nodes are lead-scope context now: the hook
# never surfaces them at session start, and the result stays put.
# ============================================================================
log "render: promise_unmet nodes -> hook silent, result left"
REPO_PM="$WORK/repo-pm"; mkdir -p "$REPO_PM/.fno"
RESULT_PM="$REPO_PM/.fno/.reconcile-result.json"
touch "$REPO_PM/.fno/.reconcile-stamp"
cat > "$RESULT_PM" <<'JSON'
{"dry_run": false, "candidates": [], "closed": [], "failures": [], "promise_unmet": [{"node_id":"x-dd1","reason":"deferred carve-out cv-99"},{"node_id":"x-dd2","reason":"close_probe failed"}]}
JSON
OUT=$(CLAUDE_PROJECT_DIR="$REPO_PM" RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
grep -q "held 2 node(s) open on the promise gate" <<<"$OUT" \
    && fail "render: hook still emits the promise-gate reminder (got: $OUT)"
[[ -f "$RESULT_PM" ]] || fail "render: hook consumed the promise_unmet result"
pass "render: hook silent on promise-gate rows; result intact"

# ============================================================================
# AC: a retryable UNKNOWN ship count is likewise routed by the notice arm,
# never rendered to the worker at session start.
# ============================================================================
log "render: promise_unknown nodes -> hook silent, result left"
REPO_PU="$WORK/repo-pu"; mkdir -p "$REPO_PU/.fno"
RESULT_PU="$REPO_PU/.fno/.reconcile-result.json"
touch "$REPO_PU/.fno/.reconcile-stamp"
cat > "$RESULT_PU" <<'JSON'
{"dry_run": false, "candidates": [], "closed": [], "failures": [], "promise_unmet": [{"node_id":"x-dd1","reason":"deferred carve-out cv-99"}], "promise_unknown": [{"node_id":"x-uu1","reason":"could not confirm 2 ships"}]}
JSON
OUT=$(CLAUDE_PROJECT_DIR="$REPO_PU" RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
grep -q "held 1 node(s) open on the promise gate" <<<"$OUT" \
    && fail "render: hook still emits the unmet line (got: $OUT)"
grep -q "could not read the ship count" <<<"$OUT" \
    && fail "render: hook still emits the unknown line (got: $OUT)"
[[ -f "$RESULT_PU" ]] || fail "render: hook consumed the promise_unknown result"
pass "render: hook silent on retryable-unknown rows; result intact"

# ============================================================================
# AC9-HP: the orphan-plan binder's result is the notice arm's input too:
# no hook-side render, no hook-side consume.
# ============================================================================
log "orphan: bound_now rows -> hook silent, result left"
REPO_OP="$WORK/repo-orphan-bound"; mkdir -p "$REPO_OP/.fno"
RESULT_OP="$REPO_OP/.fno/.orphan-plans-result.json"
touch "$REPO_OP/.fno/.reconcile-stamp"
cat > "$RESULT_OP" <<'JSON'
{"read_at":"2026-09-19T00:00:00Z","plans_dir":"/tmp/plans","rows":[{"node_id":"x-op1","plan_path":"/tmp/plans/a.md","verdict":"bound_now","detail":""},{"node_id":"x-op2","plan_path":"/tmp/plans/b.md","verdict":"bound_now","detail":""}],"counts":{"bound_now":2}}
JSON
OUT=$(CLAUDE_PROJECT_DIR="$REPO_OP" RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
grep -q "bound 2 orphan plan(s)" <<<"$OUT" \
    && fail "orphan: hook still emits the bound line (got: $OUT)"
[[ -f "$RESULT_OP" ]] || fail "orphan: hook consumed the bound result"
pass "orphan: bound_now rows stay silent; result intact for the notice arm"

# ============================================================================
# AC10-EDGE: terminal and settling rows stay silent (transient and healthy
# history never surfaces) and the quiet result is likewise untouched.
# ============================================================================
log "orphan: terminal + settling rows -> silent, untouched"
REPO_OQ="$WORK/repo-orphan-quiet"; mkdir -p "$REPO_OQ/.fno"
RESULT_OQ="$REPO_OQ/.fno/.orphan-plans-result.json"
touch "$REPO_OQ/.fno/.reconcile-stamp"
cat > "$RESULT_OQ" <<'JSON'
{"read_at":"2026-09-19T00:00:00Z","plans_dir":"/tmp/plans","rows":[{"node_id":"x-t1","plan_path":"/tmp/plans/t.md","verdict":"terminal","detail":""},{"node_id":"x-s1","plan_path":"/tmp/plans/s.md","verdict":"settling","detail":""}],"counts":{"terminal":1,"settling":1}}
JSON
OUT=$(CLAUDE_PROJECT_DIR="$REPO_OQ" RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
grep -q "orphan plan" <<<"$OUT" \
    && fail "orphan: terminal/settling rows wrongly surfaced (got: $OUT)"
[[ -f "$RESULT_OQ" ]] || fail "orphan: hook consumed the quiet result"
pass "orphan: terminal + settling rows stay silent and untouched"

# ============================================================================
# AC11-ERR: a result file that is not JSON must not kill the hook: the
# notice arm owns that file's fate, so the hook neither reads nor consumes
# it; the run still reaches reconcile_maybe_fire and exits 0.
# ============================================================================
log "orphan: non-JSON result -> hook survives and still fires"
REPO_OB="$WORK/repo-orphan-bad"; mkdir -p "$REPO_OB/.fno"
RESULT_OB="$REPO_OB/.fno/.orphan-plans-result.json"
cat > "$RESULT_OB" <<'TEXT'
not json at all
TEXT
: > "$FNO_CALL_LOG"
OUT=$(CLAUDE_PROJECT_DIR="$REPO_OB" RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
_RC_OB=$?
[[ "$_RC_OB" -eq 0 ]] \
    || fail "orphan/bad: hook exited $_RC_OB on a non-JSON orphan result"
[[ -f "$RESULT_OB" ]] \
    || fail "orphan/bad: hook consumed the non-JSON result the notice arm owns"
wait_for_file "$REPO_OB/.fno/.reconcile-result.json" \
    || fail "orphan/bad: reconcile never fired - the orphan render killed the trigger"
pass "orphan: non-JSON result survives and the reconcile still fires"

# ============================================================================
# AC: a PROVEN-STALE canonical catchup (outcome fresh, stale true) is
# lead-scope context now: the notice arm routes it, the hook stays silent.
# ============================================================================
log "render: stale-and-fresh catchup -> hook silent"
REPO_CS="$WORK/repo-catchup-stale"; mkdir -p "$REPO_CS/.fno"
RESULT_CS="$REPO_CS/.fno/.reconcile-result.json"
touch "$REPO_CS/.fno/.reconcile-stamp"
cat > "$RESULT_CS" <<'JSON'
{"dry_run": false, "candidates": [], "closed": [], "failures": [], "sync_catchup": {"outcome": "fresh", "stale": true, "pr_number": null, "swept": 0, "detail": "local default branch 5 behind origin"}}
JSON
OUT=$(CLAUDE_PROJECT_DIR="$REPO_CS" RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
grep -q "canonical-sync catch-up" <<<"$OUT" \
    && fail "render: hook still emits the catchup line (got: $OUT)"
[[ -f "$RESULT_CS" ]] || fail "render: hook consumed the catchup result"
pass "render: proven-stale catchup stays silent; result intact"

# ============================================================================
# AC: render - a result written BEFORE the `stale` key existed (outcome fresh,
# no stale key) must stay silent AND still reach the fire below. `.stale` on
# such a file is null, and `null == true` is a plain false in jq, never the
# type error the legacy case above documents.
# ============================================================================
log "render: pre-stale catchup result -> silent, trigger intact"
REPO_PC="$WORK/repo-prestale"; mkdir -p "$REPO_PC/.fno"
RESULT_PC="$REPO_PC/.fno/.reconcile-result.json"
cat > "$RESULT_PC" <<'JSON'
{"dry_run": false, "candidates": [], "closed": [], "failures": [], "sync_catchup": {"outcome": "fresh", "pr_number": null, "swept": 0, "detail": "x"}}
JSON
: > "$FNO_CALL_LOG"
OUT=$(CLAUDE_PROJECT_DIR="$REPO_PC" RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
_RC_PC=$?
[[ "$_RC_PC" -eq 0 ]] \
    || fail "render/pre-stale: hook exited $_RC_PC on a result with no stale key"
grep -q "canonical-sync catch-up" <<<"$OUT" \
    && fail "render: pre-stale result wrongly emitted a catchup line (got: $OUT)"
# No stamp => the hook must reach reconcile_maybe_fire and fire. The sweep
# is detached, so poll the call log: the heredoc above left a non-empty
# result file, so wait_for_file would pass before any chain spawns.
wait_for_log_line "backlog reconcile --json" \
    || fail "render/pre-stale: reconcile never fired"
pass "render: pre-stale result is silent and the reconcile still fires"

# ============================================================================
# AC: a legacy result (no `sync_catchup` key) must not kill the hook. The
# old render block ran under `set -euo pipefail` ABOVE the load-bearing
# reconcile trigger, so a jq type error there took out both the consume and
# every future sweep: `null | test(...)` is an ERROR (exit 5), not an empty
# match, and a bare `cu=$(...)` propagates it. Symptom was silent and
# permanent - the same stale reminder every session and no reconcile ever
# firing again on that repo. The render is gone, so this pins the trigger
# plus the result left intact for the notice arm.
# ============================================================================
log "render: result predating sync_catchup -> silent, untouched, still fires"
REPO_LEGACY="$WORK/repo-legacy"; mkdir -p "$REPO_LEGACY/.fno"
RESULT_LEGACY="$REPO_LEGACY/.fno/.reconcile-result.json"
cat > "$RESULT_LEGACY" <<'JSON'
{"dry_run": false, "candidates": [], "closed": [{"node_id":"ab-ccc333","pr_number":12}], "failures": []}
JSON
: > "$FNO_CALL_LOG"
# No stamp at all => the hook must reach reconcile_maybe_fire and fire.
OUT=$(CLAUDE_PROJECT_DIR="$REPO_LEGACY" RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
_RC_LEGACY=$?
[[ "$_RC_LEGACY" -eq 0 ]] \
    || fail "render/legacy: hook exited $_RC_LEGACY on a result with no sync_catchup"
grep -q "ab-ccc333" <<<"$OUT" \
    && fail "render/legacy: hook still renders the legacy result (got: $OUT)"
grep -q "ab-ccc333" <"$RESULT_LEGACY" \
    || fail "render/legacy: hook disturbed the result the notice arm must read"
# The stamp is a `touch`, so wait_for_file (which requires NON-EMPTY) is the
# wrong probe for it; the call log is what proves the trigger actually ran.
[[ -f "$REPO_LEGACY/.fno/.reconcile-stamp" ]] \
    || fail "render/legacy: no throttle stamp - the hook died before the trigger"
# The sweep is detached, so poll the call log (the same probe the fire case
# above uses): the chain spawns asynchronously, so an instant grep races it.
wait_for_log_line "backlog reconcile --json" \
    || fail "render/legacy: reconcile never fired - a cosmetic line killed the trigger"
pass "render: legacy result is left intact and the reconcile still fires"

# ============================================================================
# AC: _reconcile_mtime must return DIGITS on either stat dialect.
# GNU `stat -f` is --file-system, not a format flag, so `stat -f %m FILE`
# SUCCEEDS on Linux and prints a block starting `  File: ...`. A BSD-first
# `||` chain therefore never falls through to `-c %Y`, and the caller feeds
# that text to `$(( ))` where bash reads `File` as a variable name:
# "File: unbound variable" under the hook's set -u, killing the whole
# SessionStart reconcile on every Linux host that has a stamp. The macOS-only
# local run could never see it, which is why this asserts the ANSWER shape
# rather than the exit code.
# ============================================================================
log "mtime: returns digits under a GNU-shaped stat"
MT_BIN="$WORK/gnu-stat-bin"; mkdir -p "$MT_BIN"
cat > "$MT_BIN/stat" <<'GNUSTAT'
#!/usr/bin/env bash
# GNU-alike: -c takes the format; -f is --file-system and exits 0 with TEXT.
if [[ "${1:-}" == "-c" ]]; then
  shift
  if [[ "${1:-}" == "%Y" ]]; then echo 1700000000; exit 0; fi
  exit 1
fi
if [[ "${1:-}" == "-f" ]]; then
  printf '  File: "/dev/disk1"\n    ID: 0\n'
  exit 0
fi
exit 1
GNUSTAT
chmod +x "$MT_BIN/stat"

# A probe script rather than an inline `bash -uc`: the nested quoting is the
# kind of thing that fails for its own reasons and reads as a real failure.
cat > "$WORK/mtime-probe.sh" <<PROBE
set -u
source "$THROTTLE_LIB"
_t="\$(mktemp)"; : > "\$_t"
_reconcile_mtime "\$_t"
PROBE

_MT_GNU="$(PATH="$MT_BIN:$PATH" bash "$WORK/mtime-probe.sh" 2>&1)"
[[ "$_MT_GNU" =~ ^[0-9]+$ ]] \
    || fail "mtime: non-numeric under a GNU-shaped stat (got: $_MT_GNU)"
pass "mtime: digits under a GNU-shaped stat"

log "mtime: returns digits under the native stat too"
_MT_NATIVE="$(bash "$WORK/mtime-probe.sh" 2>&1)"
[[ "$_MT_NATIVE" =~ ^[0-9]+$ ]] \
    || fail "mtime: non-numeric on the native stat (got: $_MT_NATIVE)"
pass "mtime: digits on the native stat"

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
wait_for_log_line "^retro drain-postmortems$" \
    || fail "chain-fail: detached chain never completed after retro failure"
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
grep -q "retro: 2 sentinel(s) pending harvest" <<<"$OUT" \
    || fail "advisory: missing '2 sentinel(s) pending harvest' line (got: $OUT)"
pass "advisory: pending count line renders"

log "advisory: zero sentinels is silent"
REPO_ADV0="$WORK/repo-adv0"; mkdir -p "$REPO_ADV0/.fno"
touch "$REPO_ADV0/.fno/.reconcile-stamp"
EMPTY_DIR="$WORK/pending-empty"; mkdir -p "$EMPTY_DIR"
OUT=$(CLAUDE_PROJECT_DIR="$REPO_ADV0" RETRO_PENDING_DIR="$EMPTY_DIR" \
      RECONCILE_THROTTLE_SECONDS=900 bash "$HOOK" 2>/dev/null)
grep -q "pending harvest" <<<"$OUT" \
    && fail "advisory: emitted a line with zero pending sentinels (got: $OUT)"
pass "advisory: zero sentinels is silent"

echo "[reconcile-ss] all reconcile-session-start tests passed"
exit 0
