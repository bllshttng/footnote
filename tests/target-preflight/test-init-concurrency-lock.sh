#!/usr/bin/env bash
# Tests for the per-worktree init lock added to init-target-state.sh
# (ab-efcde945 follow-on, concurrent-init race fix).
#
# Two terminals in the same worktree can both legitimately pass the
# location gate. Without the lock, both write the temp file and `mv`
# sequentially; the loser silently overwrites the winner's state. The
# lock makes only one win at a time; the loser exits 75 (EX_TEMPFAIL).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INIT_SCRIPT="$REPO_ROOT/hooks/helpers/init-target-state.sh"

[[ -f "$INIT_SCRIPT" ]] || { echo "FAIL: $INIT_SCRIPT missing" >&2; exit 1; }

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP_BASE="$(mktemp -d -t target-init-lock-XXXXXX)"
trap 'rm -rf "$TMP_BASE"' EXIT

make_repo() {
    local dir="$1"
    local branch="$2"
    mkdir -p "$dir"
    (
        cd "$dir"
        git init -q -b "$branch" 2>/dev/null || { git init -q; git checkout -q -b "$branch"; }
        git config user.email "test@test.com"
        git config user.name "Test"
        echo "# test" > README.md
        git add README.md
        git commit -q -m "init"
    )
}

run_init() {
    local cwd="$1"
    shift
    (
        cd "$cwd"
        unset TARGET_START TARGET_INPUT TARGET_PLAN_PATH TARGET_LOCATION_OK TARGET_SIZE
        env TARGET_START=1 CLAUDE_PLUGIN_ROOT="$REPO_ROOT" "$@" bash "$INIT_SCRIPT" 2>&1
    )
    return $?
}

echo "=== test-init-concurrency-lock ==="

# --- AC1: clean init creates the lock, then removes it on exit -------------
echo ""
echo "--- AC1: lock created during init and cleaned up after ---"
T="$TMP_BASE/ac1-clean"
make_repo "$T" "feature/x"
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC1: clean init succeeds"
else
    fail "AC1: expected exit 0, got $EC. Output: $OUT"
fi
if [[ ! -e "$T/.fno/.init.lock" ]]; then
    pass "AC1: lock file removed by EXIT trap"
else
    fail "AC1: lock file still present after init exit"
fi

# --- AC2: live lock blocks a second init -----------------------------------
echo ""
echo "--- AC2: live lock refuses second invocation ---"
T="$TMP_BASE/ac2-live"
make_repo "$T" "feature/x"
mkdir -p "$T/.fno"
# Plant a live lock: our own PID + our own lstart provenance. The
# liveness check now also compares provenance, so writing JUST the PID
# (the old format) would be classified as "stale" by the new code
# because the recorded provenance ("") wouldn't match the live process's
# actual lstart. We write the full two-line payload.
LIVE_PROV=$(ps -p $$ -o lstart= 2>/dev/null | tr -s '[:space:]' ' ' | sed -e 's/^ //' -e 's/ $//')
printf '%s\n%s\n' "$$" "$LIVE_PROV" > "$T/.fno/.init.lock"
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -eq 75 ]]; then
    pass "AC2: exit 75 (EX_TEMPFAIL) when lock held by live PID"
else
    fail "AC2: expected exit 75, got $EC. Output: $OUT"
fi
if echo "$OUT" | grep -q "another init-target-state is running here"; then
    pass "AC2: refusal message present"
else
    fail "AC2: refusal message missing. Got: $OUT"
fi
if [[ ! -f "$T/.fno/target-state.md" ]]; then
    pass "AC2: target-state.md NOT written when lock held"
else
    fail "AC2: target-state.md written despite live lock"
fi
# Cleanup so the next test's tempdir teardown doesn't see leftover state
rm -f "$T/.fno/.init.lock"

# --- AC3: stale lock (dead PID) gets reclaimed and init proceeds ----------
echo ""
echo "--- AC3: stale lock reclaimed ---"
T="$TMP_BASE/ac3-stale"
make_repo "$T" "feature/x"
mkdir -p "$T/.fno"
# Find a PID that's guaranteed dead: pick a high number that exceeds the
# kernel's PID_MAX cap on most systems (default macOS PID_MAX is 99999,
# Linux default is 32768). We pick 999999 which is above both. If a real
# process happens to have this PID, the test is no worse than running
# without the assertion — it would falsely fail rather than falsely pass.
STALE_PID=999999
# Sanity check: confirm the PID is actually dead before relying on it.
if ps -p "$STALE_PID" >/dev/null 2>&1; then
    echo "  SKIP AC3: PID $STALE_PID happens to be live; cannot test stale-lock reclaim deterministically" >&2
else
    # Two-line stale lock: dead PID + garbage provenance. The PID is
    # dead so liveness check fails regardless of provenance.
    printf '%s\nold-fake-provenance\n' "$STALE_PID" > "$T/.fno/.init.lock"
    OUT=$(run_init "$T" 2>&1)
    EC=$?
    if [[ $EC -eq 0 ]]; then
        pass "AC3: exit 0 — stale lock reclaimed and init proceeded"
    else
        fail "AC3: expected exit 0 after reclaim, got $EC. Output: $OUT"
    fi
    if echo "$OUT" | grep -q "reclaimed stale init lock"; then
        pass "AC3: reclaim message present in stderr"
    else
        fail "AC3: reclaim message missing. Got: $OUT"
    fi
    if [[ -f "$T/.fno/target-state.md" ]]; then
        pass "AC3: state file written after reclaim"
    else
        fail "AC3: state file missing after reclaim"
    fi
    if [[ ! -e "$T/.fno/.init.lock" ]]; then
        pass "AC3: lock file removed after successful init"
    else
        fail "AC3: lock file still present after successful init"
    fi
fi

# --- AC4: lock file with empty content is treated as stale ----------------
echo ""
echo "--- AC4: empty lock file is treated as stale ---"
T="$TMP_BASE/ac4-empty"
make_repo "$T" "feature/x"
mkdir -p "$T/.fno"
# An empty lock file (interrupted write, manual `touch`) shouldn't pin
# the lock indefinitely. With the noclobber-file design this case is
# narrower than it was under mkdir+pid because the PID is written in the
# same atomic op as the create, so empty content only happens via
# external intervention. We still treat it as stale and reclaim.
: > "$T/.fno/.init.lock"
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC4: exit 0 with empty lock file (treated as stale)"
else
    fail "AC4: expected exit 0 with empty lock file, got $EC. Output: $OUT"
fi

# --- AC5: structural — atomic acquire + provenance + ps liveness ----------
# Lock in the design choices the review chain has hardened. Catches a
# regression where someone refactors back to a partial-write-vulnerable
# acquire, drops the PID-reuse provenance check, or reintroduces the
# EPERM/ESRCH-ambiguous kill -0.
echo ""
echo "--- AC5: structural assertions ---"
# Precompute the script's non-comment lines ONCE into a variable, then match
# with here-strings below. A `grep -vE … | grep -q …` pipeline flakes under
# `set -o pipefail`: grep -q closes the pipe on first match, the upstream grep
# gets SIGPIPE (141), and pipefail reports the whole pipeline as failed.
_AC5_CODE=$(grep -vE '^[[:space:]]*#' "$INIT_SCRIPT")
# Atomic acquire via temp-file + hardlink. The `ln` call is what makes
# the lock visible exactly when its contents are visible (no partial-
# write window from a noclobber-write race).
if grep -qE 'ln "\$tmp" "\$INIT_LOCK_FILE"' <<<"$_AC5_CODE"; then
    pass "AC5: lock acquisition uses temp-file + hardlink (no partial-write window)"
else
    fail "AC5: hardlink-from-tempfile acquisition pattern missing"
fi
# Provenance comparison (PID + lstart) prevents PID-reuse false-positives.
if grep -q "_init_process_provenance" "$INIT_SCRIPT" \
   && grep -q "_init_process_holds_lock" "$INIT_SCRIPT"; then
    pass "AC5: provenance helpers present (prevents PID-reuse false positives)"
else
    fail "AC5: provenance helpers missing — PID-reuse risk reintroduced"
fi
# Match `kill -0` only in code lines (lines that don't start with `#` after
# leading whitespace). The script contains a comment explaining WHY we
# don't use kill -0 — that mention should not trip this assertion.
if ! grep -q "kill -0" <<<"$_AC5_CODE"; then
    pass "AC5: liveness check uses ps -p, not kill -0 (avoids EPERM/ESRCH ambiguity)"
else
    fail "AC5: init script still uses kill -0 for liveness (EPERM/ESRCH cannot be disambiguated in bash)"
fi

# --- AC6: trap survives a single-quote in the lock path ------------------
# Codex round 3 (PR #321 P1): the previous form
#   trap "rm -f '$INIT_LOCK_FILE'" EXIT
# embedded the path raw inside single quotes. Any single quote in the
# path (legal in Unix; e.g. `/tmp/bob's-tmp/`) broke the trap's quoting,
# exited non-zero, and left the lock file behind. The fix uses a trap
# function so the path is reached via parameter expansion at fire time.
echo ""
echo "--- AC6: trap survives single quote in path ---"
# mktemp -d with an XXXXXX template doesn't include single quotes by
# itself, so we explicitly construct a path with a literal apostrophe.
T_PARENT="$TMP_BASE/ac6-quote-test"
T="$T_PARENT/bob's-tmp"
mkdir -p "$T"
make_repo "$T" "feature/x"
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC6: init exits 0 when lock path contains a single quote"
else
    fail "AC6: init exited $EC with apostrophe in path. Output: $OUT"
fi
# Lock must be cleaned up by trap. Pre-fix: lock would remain because
# trap body failed to parse.
if [[ ! -e "$T/.fno/.init.lock" ]]; then
    pass "AC6: lock file removed on exit despite quote in path"
else
    fail "AC6: lock file left behind — trap body failed to expand path correctly"
fi
# State file must exist (init completed normally).
if [[ -f "$T/.fno/target-state.md" ]]; then
    pass "AC6: state file created normally with quoted path"
else
    fail "AC6: state file missing — init aborted before write"
fi

# --- AC7: trap registration uses a function, not an inlined command ------
# Structural assert: catches a regression where someone refactors back
# to the `trap "rm -f '$VAR'" EXIT` form. The trap line must NOT contain
# a `'` character (which would indicate inlined-with-quotes embedding).
echo ""
echo "--- AC7: trap uses function reference, not inlined path ---"
TRAP_LINE=$(grep -E "^[[:space:]]*trap[[:space:]]+_init_release_lock" "$INIT_SCRIPT" | head -1)
if [[ -n "$TRAP_LINE" ]]; then
    pass "AC7: EXIT trap registered as a function reference"
else
    fail "AC7: EXIT trap does not use _init_release_lock function — likely regressed to inline form"
fi

# --- AC8b: lock-read failure doesn't abort under set -e -------------------
# Codex round 6 (PR #321 P2): the sed substitutions that read the lock
# file after acquire-fail run under `set -euo pipefail`. If the lock
# file is unreadable (vanished mid-race, broken symlink, perm error,
# etc.) sed returns non-zero and the substitution rc can propagate
# errexit out of $(...) and abort the whole script BEFORE the reclaim
# path runs. The fix is defensive `|| true` on each substitution.
#
# Deterministic simulation: plant a broken symlink as the lock. ln
# fails to acquire because the target exists (the symlink); sed -n
# follows the symlink and gets ENOENT. Both failures hit on the same
# invocation. The test passes if the script recovers via reclaim
# (rm -f removes the symlink, retry acquire succeeds) instead of
# aborting.
echo ""
echo "--- AC8b: broken-symlink lock recovers instead of aborting ---"
T="$TMP_BASE/ac8b-broken-symlink"
make_repo "$T" "feature/x"
mkdir -p "$T/.fno"
ln -s /this/path/does/not/exist "$T/.fno/.init.lock"
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC8b: broken symlink lock recovered via reclaim (sed failure didn't abort)"
else
    fail "AC8b: broken symlink lock caused abort (exit $EC) — sed substitution propagated set -e. Output: $OUT"
fi
if [[ -f "$T/.fno/target-state.md" ]]; then
    pass "AC8b: state file written after recovery"
else
    fail "AC8b: state file missing after reclaim"
fi

# Structural assert for AC8b: the sed reads must use `|| true` to
# guard against set -e propagation. Catches a regression where someone
# refactors back to undefended substitution.
if grep -q "sed -n '1p' \"\$INIT_LOCK_FILE\" 2>/dev/null || true" "$INIT_SCRIPT" \
   && grep -q "sed -n '2p' \"\$INIT_LOCK_FILE\" 2>/dev/null || true" "$INIT_SCRIPT"; then
    pass "AC8b: lock-file sed reads guarded with || true"
else
    fail "AC8b: lock-file sed reads missing '|| true' defense"
fi

# --- AC8: PID reuse (live PID, wrong provenance) is reclaimed -------------
# Codex round 5 (PR #321 P2): the previous PID-only liveness check
# misclassifies a reused PID as a live lock holder. We simulate the
# scenario: write OUR PID (alive) but a DIFFERENT lstart provenance
# (as if our PID had been freshly reassigned to this process from some
# unrelated dead process). The new code compares stored provenance
# against the live process's lstart and detects the mismatch, treating
# the lock as stale and reclaiming.
echo ""
echo "--- AC8: PID reuse detected via lstart mismatch ---"
T="$TMP_BASE/ac8-pid-reuse"
make_repo "$T" "feature/x"
mkdir -p "$T/.fno"
# Our PID is alive, but we plant a provenance that no process in the
# system has — `Thu Jan 1 00:00:00 1970` is the epoch and won't match
# any live process's actual start time on any reasonable machine.
printf '%s\nThu Jan  1 00:00:00 1970\n' "$$" > "$T/.fno/.init.lock"
OUT=$(run_init "$T" 2>&1)
EC=$?
if [[ $EC -eq 0 ]]; then
    pass "AC8: exit 0 — PID-reuse detected via provenance mismatch and reclaimed"
else
    fail "AC8: expected exit 0 (reclaim), got $EC. Output: $OUT"
fi
if echo "$OUT" | grep -q "reclaimed stale init lock"; then
    pass "AC8: reclaim message present in stderr"
else
    fail "AC8: reclaim message missing. Got: $OUT"
fi
if [[ -f "$T/.fno/target-state.md" ]]; then
    pass "AC8: state file written after reclaim"
else
    fail "AC8: state file missing after reclaim"
fi

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]]
