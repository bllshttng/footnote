#!/usr/bin/env bash
# tests/hooks/test_eval_sweep_session_start.sh
#
# Eval-sweep hygiene (x-dbdf): the SessionStart eval-loop ignition must fire at
# most once per repo per day regardless of worktree count (canonical stamp +
# singleton claim), bound every stage with a timeout, and log to
# .fno/logs/eval-sweep.log instead of /dev/null.
#
# The now-extracted helpers are exercised directly (no detach/poll flakiness):
#   US1 - _eval_sweep_canonical_root + canonical stamp placement
#   US2 - _eval_sweep_try_claim arbiter (acquired / held / degraded)
#   US3 - _eval_sweep_run_stages bounds a slow stage + writes a run header
#
# A FAKE `fno` on PATH means no real sweep, claim, or network call ever runs.
#
# Run: bash tests/hooks/test_eval_sweep_session_start.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT_REAL="$(cd "${SCRIPT_DIR}/../.." && pwd)"
THROTTLE_LIB="${REPO_ROOT_REAL}/scripts/lib/eval-sweep-throttle.sh"

log()  { printf '[eval-sweep-ss] %s\n' "$*"; }
fail() { printf '[eval-sweep-ss] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[eval-sweep-ss] PASS: %s\n' "$*"; }

[[ -f "$THROTTLE_LIB" ]] || fail "throttle lib not found at $THROTTLE_LIB"

WORK=$(mktemp -d -t eval-sweep-ss-XXXXXX)
WORK=$(cd "$WORK" && pwd -P)  # resolve macOS /var -> /private/var so git-common-dir compares equal
trap 'rm -rf "$WORK"' EXIT

# --- Fake `fno` on PATH: a claims ledger + a call log; no real work. ---------
FAKEBIN="$WORK/bin"
mkdir -p "$FAKEBIN"
export FAKE_CLAIMS="$WORK/claims"        # dir of key -> holder files
export FNO_CALL_LOG="$WORK/fno-calls.log"
mkdir -p "$FAKE_CLAIMS"
: > "$FNO_CALL_LOG"
cat > "$FAKEBIN/fno" <<'FAKE'
#!/usr/bin/env bash
echo "$*" >> "$FNO_CALL_LOG"
_slug() { echo "$1" | tr '/:' '__'; }
if [[ "${1:-}" == "claim" && "${2:-}" == "acquire" ]]; then
    key="$3"; holder=""
    shift 3
    while [[ $# -gt 0 ]]; do [[ "$1" == "--holder" ]] && holder="$2"; shift; done
    f="$FAKE_CLAIMS/$(_slug "$key")"
    if [[ -f "$f" ]]; then
        cur="$(cat "$f")"
        if [[ "$cur" != "$holder" ]]; then
            echo "claim '$key' held by $cur" >&2
            exit 0
        fi
    fi
    echo "$holder" > "$f"
    echo "{\"key\": \"$key\", \"holder\": \"$holder\"}"
    exit 0
fi
if [[ "${1:-}" == "claim" && "${2:-}" == "release" ]]; then
    key="$3"; rm -f "$FAKE_CLAIMS/$(_slug "$key")"; exit 0
fi
if [[ "${1:-}" == "loops" && "${2:-}" == "status" ]]; then echo "not paused"; exit 0; fi
exit 0
FAKE
chmod +x "$FAKEBIN/fno"
export PATH="$FAKEBIN:$PATH"

# shellcheck disable=SC1090
source "$THROTTLE_LIB"

# =========================================================================== #
# US1: canonical root + stamp placement
# =========================================================================== #

# A real repo + linked worktree so git-common-dir resolution is genuine.
CANON="$WORK/repo"
mkdir -p "$CANON"
git -C "$CANON" init -q
git -C "$CANON" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
mkdir -p "$CANON/.fno"
WT="$WORK/wt"
git -C "$CANON" worktree add -q "$WT" -b wt-branch 2>/dev/null || fail "worktree add failed"

got="$(_eval_sweep_canonical_root "$WT")"
[[ "$got" == "$CANON" ]] || fail "canonical root from worktree: got '$got' want '$CANON'"
pass "US1: worktree resolves to canonical root"

# Non-git dir falls back to itself (degrade, never crash).
NONGIT="$WORK/plain"
mkdir -p "$NONGIT"
got="$(_eval_sweep_canonical_root "$NONGIT")"
[[ "$got" == "$NONGIT" ]] || fail "non-git fallback: got '$got' want '$NONGIT'"
pass "US1: non-git dir keeps local fallback"

# eval_sweep_maybe_fire from the WORKTREE must stamp the CANONICAL .fno, and a
# second fire (from canonical, stamp now fresh) must skip - one sweep per repo.
rm -f "$CANON/.fno/.eval-sweep-stamp"
: > "$FNO_CALL_LOG"
eval_sweep_maybe_fire "$WT"
[[ -f "$CANON/.fno/.eval-sweep-stamp" ]] || fail "canonical stamp not created from worktree fire"
[[ ! -f "$WT/.fno/.eval-sweep-stamp" ]] || fail "stamp wrongly created in worktree-local .fno"
pass "US1: fire from worktree stamps the canonical .fno"

# Second fire within the window: throttle short-circuits before any claim call.
before="$(grep -c 'claim acquire' "$FNO_CALL_LOG" || true)"
eval_sweep_maybe_fire "$CANON"
after="$(grep -c 'claim acquire' "$FNO_CALL_LOG" || true)"
[[ "$after" == "$before" ]] || fail "second within-window fire attempted a claim (throttle failed)"
pass "US1: second fire within window skips (shared stamp)"

# =========================================================================== #
# US2: singleton claim arbiter
# =========================================================================== #

rm -rf "$FAKE_CLAIMS"; mkdir -p "$FAKE_CLAIMS"
r="$(_eval_sweep_try_claim fno eval-sweep:repoX holderA)"
[[ "$r" == "acquired" ]] || fail "first acquire: got '$r' want acquired"
r="$(_eval_sweep_try_claim fno eval-sweep:repoX holderB)"
[[ "$r" == "held" ]] || fail "contended acquire: got '$r' want held"
pass "US2: unique holders -> first acquires, second sees held"

# Claim layer down (fake fno that exits non-zero) -> degrade, still fire.
cat > "$FAKEBIN/fno-down" <<'DOWN'
#!/usr/bin/env bash
exit 3
DOWN
chmod +x "$FAKEBIN/fno-down"
r="$(_eval_sweep_try_claim "$FAKEBIN/fno-down" eval-sweep:repoX holderC)"
[[ "$r" == "degraded" ]] || fail "claim layer down: got '$r' want degraded"
pass "US2: claim layer down degrades to stamp-only (AC1-ERR)"

# =========================================================================== #
# US3: per-stage timeout + logging
# =========================================================================== #

# A fake abi whose `observer sweep` sleeps far past the bound; the wrapper must
# kill it near the bound, not wait it out.
SLOWBIN="$WORK/slow"
mkdir -p "$SLOWBIN"
cat > "$SLOWBIN/abi" <<'SLOW'
#!/usr/bin/env bash
case "$1 $2" in
    "observer sweep") sleep 30 ;;
    *) : ;;
esac
exit 0
SLOW
chmod +x "$SLOWBIN/abi"

LOG="$WORK/repo/.fno/logs/eval-sweep.log"
rm -f "$LOG"
EVAL_SWEEP_STAGE_TIMEOUT=1
start=$(date +%s)
_eval_sweep_run_stages "$CANON" "$SLOWBIN/abi" "$LOG" "" ""
elapsed=$(( $(date +%s) - start ))
# Two slow `observer sweep` stages, each bounded to 1s, plus two instant ticks:
# well under the unbounded 60s. Generous ceiling to avoid CI flake.
(( elapsed < 15 )) || fail "stages not bounded: elapsed ${elapsed}s (expected < 15)"
pass "US3: a wedged stage is killed at the bound (elapsed ${elapsed}s)"

[[ -f "$LOG" ]] || fail "log file not written"
grep -q '=== eval-sweep run ' "$LOG" || fail "run header missing from log"
pass "US3: run appends a header to .fno/logs/eval-sweep.log"

# Size guard: an oversized log is truncated on the next run.
EVAL_SWEEP_LOG_MAX_BYTES=100
head -c 500 /dev/zero | tr '\0' 'x' > "$LOG"
_eval_sweep_trim_log "$LOG"
sz=$(wc -c < "$LOG" | tr -d ' ')
(( sz == 0 )) || fail "size guard did not truncate oversized log (size=$sz)"
pass "US3: size guard truncates a log past the byte cap"

log "all eval-sweep-ss tests passed"
