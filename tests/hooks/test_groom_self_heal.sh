#!/usr/bin/env bash
# tests/hooks/test_groom_self_heal.sh
#
# The SessionStart grooming fallback (x-1c7b). Four grooming surfaces have
# shipped and never run; this is the trigger of last resort, so what it must NOT
# do matters as much as what it does:
#
#   AC1-EDGE - a healthy pass (>= threshold fresh) spawns nothing and writes no
#              watermark, so the LaunchAgent keeps sole ownership of the cadence
#   AC2-EDGE - N concurrent worktree sessions collapse to exactly one dispatch
#   AC1-FR   - a dispatch that fails leaves nothing claiming success
#
# A FAKE `fno` on PATH means no real grooming pass, claim, or graph write ever
# happens. NOTE: this repo has two test trees - a green `fno test cli/tests` is
# NOT evidence for this file. Run it directly.
#
# Run: bash tests/hooks/test_groom_self_heal.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT_REAL="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HELPER="${REPO_ROOT_REAL}/hooks/helpers/groom-self-heal.sh"

log()  { printf '[groom-heal] %s\n' "$*"; }
fail() { printf '[groom-heal] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[groom-heal] PASS: %s\n' "$*"; }

[[ -f "$HELPER" ]] || fail "helper not found at $HELPER"

WORK=$(mktemp -d -t groom-heal-XXXXXX)
trap 'rm -rf "$WORK"' EXIT

# --- Fake `fno`: logs every call; `--check` exit code is harness-controlled. ---
FAKEBIN="$WORK/bin"
mkdir -p "$FAKEBIN"
export FNO_CALL_LOG="$WORK/fno-calls.log"
cat > "$FAKEBIN/fno" <<'FAKE'
#!/usr/bin/env bash
echo "$*" >> "$FNO_CALL_LOG"
if [[ "${1:-}" == "backlog" && "${2:-}" == "groom" && "${3:-}" == "--check" ]]; then
    echo '{"state": "ran", "hours": 96.0}'
    exit "${FAKE_CHECK_EXIT:-0}"   # 0 = a pass is due
fi
if [[ "${1:-}" == "backlog" && "${2:-}" == "groom" ]]; then
    # The dispatch itself. Slow enough that a caller which waited would show it.
    exit "${FAKE_GROOM_EXIT:-0}"
fi
exit 0
FAKE
chmod +x "$FAKEBIN/fno"
export PATH="$FAKEBIN:$PATH"

TODAY="$(date -u +%Y-%m-%d)"

# Count dispatches (a `backlog groom` call with no --check), tolerating the
# backgrounded subshell by polling briefly.
# `grep -c` exits 1 on zero matches, so its count is captured and the exit
# status discarded - an `|| echo 0` fallback would emit a SECOND line.
count_lines() {
    local n
    n="$(grep -c -- "$1" "$FNO_CALL_LOG" 2>/dev/null)" || true
    printf '%s' "${n:-0}"
}
dispatch_count() { count_lines '^backlog groom$'; }
settle() { sleep 0.5; }

fresh_project() {
    local dir="$WORK/$1"
    rm -rf "$dir"; mkdir -p "$dir/.fno"
    : > "$FNO_CALL_LOG"
    echo "$dir"
}

# --- AC1-EDGE: a healthy pass stays dormant -----------------------------------
proj="$(fresh_project healthy)"
( cd "$proj" && FAKE_CHECK_EXIT=1 bash "$HELPER" )
settle
[[ "$(dispatch_count)" == "0" ]] || fail "AC1-EDGE: a fresh pass must not dispatch"
if compgen -G "$proj/.fno/.groom-heal-*" >/dev/null; then
    fail "AC1-EDGE: a fresh pass must not write a watermark (it would burn the day)"
fi
pass "AC1-EDGE: healthy grooming spawns nothing and writes no watermark"

# --- happy path: a stale pass dispatches once and watermarks ------------------
proj="$(fresh_project stale)"
( cd "$proj" && bash "$HELPER" )
settle
[[ "$(dispatch_count)" == "1" ]] || fail "a stale pass must dispatch exactly once"
[[ -e "$proj/.fno/.groom-heal-${TODAY}" ]] || fail "the winner must write today's watermark"
pass "stale grooming dispatches once and claims the day"

# --- the watermark is a day gate: a second session the same day is a no-op ----
: > "$FNO_CALL_LOG"
( cd "$proj" && bash "$HELPER" )
settle
[[ "$(dispatch_count)" == "0" ]] || fail "a second session the same day must not re-dispatch"
[[ "$(count_lines "\-\-check")" == "0" ]] \
    || fail "the watermark must short-circuit BEFORE the freshness probe"
pass "today's watermark short-circuits before the probe"

# --- AC2-EDGE: concurrent sessions collapse to one dispatch -------------------
proj="$(fresh_project concurrent)"
for _ in 1 2 3 4 5; do
    ( cd "$proj" && bash "$HELPER" ) &
done
wait
settle
n="$(dispatch_count)"
[[ "$n" == "1" ]] || fail "AC2-EDGE: 5 concurrent sessions dispatched $n times, want 1"
pass "AC2-EDGE: five concurrent sessions dispatch exactly once"

# --- AC1-FR: a failing dispatch claims nothing --------------------------------
proj="$(fresh_project failing)"
( cd "$proj" && FAKE_GROOM_EXIT=1 bash "$HELPER" )
settle
# The helper must exit 0 regardless (it is advisory and must never block a
# session), and must not report success anywhere - the marker not advancing is
# what keeps the failure visible on the next `fno doctor`.
( cd "$proj" && FAKE_GROOM_EXIT=1 bash "$HELPER" ) || fail "AC1-FR: helper must never exit nonzero"
pass "AC1-FR: a failed dispatch neither blocks the session nor reports success"

# --- degradation: no fno on PATH is silent and clean --------------------------
proj="$(fresh_project no-fno)"
( cd "$proj" && PATH="/usr/bin:/bin" bash "$HELPER" ) || fail "missing fno must exit 0"
if compgen -G "$proj/.fno/.groom-heal-*" >/dev/null; then
    fail "missing fno must not write a watermark"
fi
pass "no fno on PATH degrades silently"

log "all groom self-heal tests passed"
