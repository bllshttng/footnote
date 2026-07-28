#!/usr/bin/env bash
# Unit tests for scripts/lib/with-timeout.sh.
#
# The two injection-hook suites cover the cap end-to-end through a hook. This
# covers the helper directly, because three of its properties are invisible from
# a hook's output and each one was a live defect in a previous implementation:
# the fast path must not pay the bound, no orphan `sleep` may survive a call, and
# nothing may reach the caller's stderr.
#
# Runs under /bin/bash explicitly. bash 3.2 is the floor this has to hold on
# (stock macOS), and it is also the shell whose job-control notices leak.
#
# Exit codes: 0 all passed, 1 assertion failed, 77 skipped.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LIB="$REPO_ROOT/scripts/lib/with-timeout.sh"

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

[[ -f "$LIB" ]] || { echo "FAIL: helper not found at $LIB" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "SKIP: python3 not on PATH" >&2; exit 77; }

bash -n "$LIB" || fail "bash -n rejected $LIB"

TMP="$(mktemp -d -t with-timeout-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# A hung command with a distinctive bound, and a fast one. `hang` sleeps well
# past its bound; `wrapped` puts a shell between the helper and the sleep, which
# is the case a single-pid kill fails to cap.
cat > "$TMP/hang" <<'EOF'
#!/usr/bin/env bash
sleep 30
EOF
cat > "$TMP/wrapped" <<'EOF'
#!/usr/bin/env bash
bash -c 'sleep 30; echo late'
EOF
cat > "$TMP/fast" <<'EOF'
#!/usr/bin/env bash
echo "ok-payload"
exit 3
EOF
chmod +x "$TMP/hang" "$TMP/wrapped" "$TMP/fast"

# Each case runs in its own /bin/bash so `set -m` state cannot leak between them.
# stdout carries "elapsed|rc|output"; stderr is kept separate so an assertion can
# prove it stayed empty.
probe() {
    local secs="$1" cmd="$2"
    /bin/bash -c '
        set -uo pipefail
        source "$1"
        export PATH="$2:$PATH"
        s=$(python3 -c "import time; print(int(time.time()*1000))")
        out=$(with_timeout "$3" "$4" 2>/dev/null || true)
        e=$(python3 -c "import time; print(int(time.time()*1000))")
        printf "%s|%s\n" "$((e - s))" "$out"
    ' _ "$LIB" "$TMP" "$secs" "$cmd"
}

# 1. A hung command is capped near its bound, not run to completion.
res="$(probe 2 hang 2>/dev/null)"
elapsed="${res%%|*}"
if [[ "$elapsed" -ge 1500 && "$elapsed" -le 5000 ]]; then
    pass "hung command capped at the bound (${elapsed}ms, not 30000ms)"
else
    fail "hung command not capped: ${elapsed}ms (expected 1500-5000ms)"
fi

# 2. Same, with a shell wrapper between helper and sleep. A single-pid kill
#    leaves the grandchild holding the capture pipe and the caller blocks anyway.
res="$(probe 2 wrapped 2>/dev/null)"
elapsed="${res%%|*}"
if [[ "$elapsed" -ge 1500 && "$elapsed" -le 5000 ]]; then
    pass "wrapped hung command capped too (${elapsed}ms) - the whole group dies"
else
    fail "wrapped hung command not capped: ${elapsed}ms (grandchild survived the signal)"
fi

# 3. The fast path pays neither the bound nor a truncated payload, and the
#    command's own exit status survives.
res="$(probe 30 fast 2>/dev/null)"
elapsed="${res%%|*}"
out="${res#*|}"
[[ "$elapsed" -lt 1000 ]] \
    && pass "fast path not delayed by the watchdog (${elapsed}ms)" \
    || fail "fast path took ${elapsed}ms with a 30s bound (watchdog is holding the pipe)"
[[ "$out" == "ok-payload" ]] \
    && pass "fast path output captured intact" \
    || fail "fast path output was [$out], expected [ok-payload]"

/bin/bash -c 'set -uo pipefail; source "$1"; export PATH="$2:$PATH"; with_timeout 30 fast >/dev/null 2>&1' _ "$LIB" "$TMP"
rc=$?
[[ $rc -eq 3 ]] \
    && pass "command's own exit status propagates (rc=3)" \
    || fail "expected rc=3 from the command, got rc=$rc"

# 2b. A child that IGNORES SIGTERM is still bounded. TERM alone is not a cap:
#     `wait` has no remaining deadline after it, so such a child leaves the
#     caller stuck forever, which is worse than no cap at all. This is the one
#     case that must be bounded externally too, because a regression here hangs
#     rather than fails, and a hanging test in CI is its own outage.
cat > "$TMP/stubborn" <<'EOF'
#!/usr/bin/env bash
trap '' TERM
while :; do sleep 0.2; done
EOF
chmod +x "$TMP/stubborn"

/bin/bash -c '
    set -uo pipefail
    source "$1"
    export PATH="$2:$PATH"
    s=$(python3 -c "import time; print(int(time.time()*1000))")
    with_timeout 2 stubborn >/dev/null 2>&1
    e=$(python3 -c "import time; print(int(time.time()*1000))")
    printf "%s\n" "$((e - s))" > "$3"
' _ "$LIB" "$TMP" "$TMP/stubborn-ms" >/dev/null 2>&1 &
probe_pid=$!
( sleep 12; kill -KILL "$probe_pid" 2>/dev/null ) >/dev/null 2>&1 &
guard_pid=$!
wait "$probe_pid" 2>/dev/null; probe_rc=$?
kill -TERM -"$guard_pid" 2>/dev/null; wait "$guard_pid" 2>/dev/null
pkill -KILL -f "$TMP/stubborn" >/dev/null 2>&1

stubborn_ms="$(cat "$TMP/stubborn-ms" 2>/dev/null || echo "")"
if [[ -z "$stubborn_ms" ]]; then
    fail "a SIGTERM-ignoring child was never bounded (rc=$probe_rc): with_timeout did not return and the external guard had to kill it"
elif [[ "$stubborn_ms" -ge 1500 && "$stubborn_ms" -le 8000 ]]; then
    pass "SIGTERM-ignoring child escalated to KILL and bounded (${stubborn_ms}ms)"
else
    fail "SIGTERM-ignoring child returned in ${stubborn_ms}ms, outside the 2s bound plus grace"
fi

# 3b. A non-integer bound is refused, not silently turned into a kill at t=0.
#     GNU timeout accepted `30m`; `sleep` on stock macOS does not, and the one
#     operator-settable bound in the tree (EVAL_SWEEP_STAGE_TIMEOUT) sits two
#     lines from a sibling knob that defaults to `30m`.
badout=$(/bin/bash -c 'set -uo pipefail; source "$1"; export PATH="$2:$PATH"; with_timeout 30m fast 2>/dev/null' _ "$LIB" "$TMP" 2>/dev/null)
badrc=$?
[[ $badrc -eq 2 ]] \
    && pass "non-integer bound refused (rc=2), not treated as an expired timer" \
    || fail "expected rc=2 for a non-integer bound, got rc=$badrc"
[[ -z "$badout" ]] \
    && pass "refused bound runs nothing" \
    || fail "a refused bound still ran the command: $badout"

# 4. Nothing reaches the caller's stderr. An unwaited job killed by a signal
#    makes bash announce "Terminated" on whoever sourced the helper.
errfile="$TMP/err"
/bin/bash -c '
    set -uo pipefail
    source "$1"
    export PATH="$2:$PATH"
    out=$(with_timeout 2 hang 2>/dev/null || true)
    # A second call, so a notice deferred to the next command boundary has one.
    out=$(with_timeout 2 fast 2>/dev/null || true)
' _ "$LIB" "$TMP" 2>"$errfile"
if [[ -s "$errfile" ]]; then
    fail "helper wrote to the caller's stderr: $(tr '\n' ' ' < "$errfile")"
else
    pass "no job-control noise on the caller's stderr"
fi

# 5. No orphan `sleep` outlives a fast call. This is the incident
#    scripts/lib/eval-sweep-throttle.sh was written to prevent; the bound here is
#    deliberately long so a surviving watchdog sleep is unmistakable.
if command -v pgrep >/dev/null 2>&1; then
    /bin/bash -c 'set -uo pipefail; source "$1"; export PATH="$2:$PATH"; with_timeout 47 fast >/dev/null 2>&1' _ "$LIB" "$TMP"
    survivors="$(pgrep -f 'sleep 47' 2>/dev/null | wc -l | tr -d ' ')"
    if [[ "$survivors" == "0" ]]; then
        pass "watchdog sleep reaped, no orphan survives a fast call"
    else
        fail "$survivors orphaned 'sleep 47' process(es) survived the call"
        pkill -f 'sleep 47' 2>/dev/null
    fi
else
    echo "  note: pgrep absent, orphan-reaping assertion not run"
fi

# 6. Exactly one implementation of the bound exists. No behavioral test on one
#    caller can cover this: the defect being fixed was six implementations of one
#    operation, five of which capped nothing, and two of those were found only by
#    running this grep by hand. The positive control matters as much as the
#    absence - an empty search result is a claim (AGENTS.md).
defs="$(grep -rlE '^[[:space:]]*(_?with_timeout|_timeout)\(\)' "$REPO_ROOT/hooks" "$REPO_ROOT/scripts" --include='*.sh' 2>/dev/null | sort)"
if [[ "$defs" == "$REPO_ROOT/scripts/lib/with-timeout.sh" ]]; then
    pass "exactly one timeout implementation, and it is the shared lib"
else
    fail "expected only scripts/lib/with-timeout.sh to define the bound, found: ${defs:-<nothing, so this grep is broken>}"
fi

# A coreutils preference chain in front of the helper reintroduces the two-paths
# defect: the bash path then goes untested on every host that has the binary.
# Scoped to all of scripts/, not just scripts/lib/: check-event-schema-parity.sh
# is a direct caller and lives in neither hooks/ nor scripts/lib/, so a narrower
# sweep passes with a chain reintroduced in the one caller that has no
# behavioral coverage of its bound.
chain="$(grep -rlE 'command -v g?timeout' "$REPO_ROOT/hooks" "$REPO_ROOT/scripts" --include='*.sh' 2>/dev/null | sort)"
if [[ -z "$chain" ]]; then
    pass "no timeout/gtimeout preference chain remains in hooks/ or scripts/"
else
    fail "timeout preference chain reintroduced in: $chain"
fi

echo ""
echo "with-timeout: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
