#!/usr/bin/env bash
# Tests for scripts/lib/assert-absent.sh
# AC3-EDGE is the positive control on the helper itself: a probe zero with a
# FAILED control must refuse, not pass. A helper that silently matched
# nothing would still read as success.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPER="$SCRIPT_DIR/../../scripts/lib/assert-absent.sh"
PASS=0
FAIL=0

if [[ ! -f "$HELPER" ]]; then
    echo "FAIL: $HELPER not found"
    exit 1
fi

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

# Hermetic temp repo: every helper run happens inside it, never the real tree.
REPO="$(mktemp -d)"
trap 'rm -rf "$REPO"' EXIT
(
    cd "$REPO" || exit 1
    git init -q
    git config user.email test@example.com
    git config user.name test
    printf 'known-token\nother line\n' > f.txt
    git add f.txt
    git commit -qm init
)
cd "$REPO" || exit 1

# Run the helper from inside the temp repo; capture stdout/stderr to files.
run() {
    bash "$HELPER" "$@" > "$REPO/stdout" 2> "$REPO/stderr"
    echo $?
}

assert_no_marker() {
    if grep -q '^assert-absent: absent' "$REPO/stdout"; then
        fail "$1: absent marker printed on a refusal"
    else
        pass "$1: no absent marker on refusal"
    fi
}

# ---- AC1-HP: control hits, probe zero -> exit 0 with the marker ----
echo "AC1-HP: clean absence with a live control"
RC=$(run --control known-token --probe missing-token -- git grep -n -e {})
if [[ $RC -eq 0 ]]; then pass "rc=0"; else fail "rc=$RC (expected 0)"; fi
if grep -q '^assert-absent: absent probe=missing-token control=known-token control_hits=1$' "$REPO/stdout"; then
    pass "marker line with control_hits=1"
else
    fail "marker line missing or wrong: $(cat "$REPO/stdout")"
fi

# ---- AC2-HP: probe hits -> exit 1, hit lines on stdout ----
echo "AC2-HP: probe hit reports the hits"
RC=$(run --control known-token --probe 'other line' -- git grep -n -e {})
if [[ $RC -eq 1 ]]; then pass "rc=1"; else fail "rc=$RC (expected 1)"; fi
if grep -q '^f.txt:2:other line$' "$REPO/stdout"; then
    pass "hit line on stdout"
else
    fail "hit line missing: $(cat "$REPO/stdout")"
fi

# ---- AC3-EDGE: broken instrument (fixed-string mode, regex control) ----
echo "AC3-EDGE: zero probe with a failed control refuses"
RC=$(run --control 'known.*token' --probe 'known.*token' -- git grep -nF -e {})
if [[ $RC -eq 2 ]]; then pass "rc=2"; else fail "rc=$RC (expected 2)"; fi
if grep -q 'instrument broken' "$REPO/stderr"; then
    pass "stderr names the broken instrument"
else
    fail "stderr missing 'instrument broken': $(cat "$REPO/stderr")"
fi
assert_no_marker "AC3-EDGE"

# ---- AC4-EDGE: no control at all, and {} placeholder count ----
echo "AC4-EDGE: absence with no control is not a verdict"
RC=$(run --probe missing-token -- git grep -n -e {})
if [[ $RC -eq 2 ]]; then pass "rc=2 without --control"; else fail "rc=$RC (expected 2)"; fi
assert_no_marker "AC4-EDGE/no-control"

RC=$(run --control known-token --probe missing-token -- git grep -n -e)
if [[ $RC -eq 2 ]]; then pass "rc=2 with zero {}"; else fail "rc=$RC (expected 2)"; fi
assert_no_marker "AC4-EDGE/zero-placeholder"

RC=$(run --control known-token --probe missing-token -- git grep -n -e {} -e {})
if [[ $RC -eq 2 ]]; then pass "rc=2 with two {}"; else fail "rc=$RC (expected 2)"; fi
assert_no_marker "AC4-EDGE/two-placeholders"

# ---- AC5-EDGE: typo'd scope and a tool error ----
echo "AC5-EDGE: typo'd scope reads as a broken instrument, not a pass"
RC=$(run --control known-token --probe missing-token -- git grep -n -e {} -- no-such-dir)
if [[ $RC -eq 2 ]]; then pass "rc=2 on typo'd scope"; else fail "rc=$RC (expected 2)"; fi
assert_no_marker "AC5-EDGE/typo-scope"

echo "AC5-EDGE: tool error (exit above 1) refuses"
RC=$(run --control known-token --probe missing-token -- grep -rn -e {} /nonexistent-assert-absent-path)
if [[ $RC -eq 2 ]]; then pass "rc=2 on tool error"; else fail "rc=$RC (expected 2)"; fi
if grep -q 'tool error' "$REPO/stderr"; then
    pass "stderr names the tool error"
else
    fail "stderr missing 'tool error': $(cat "$REPO/stderr")"
fi
assert_no_marker "AC5-EDGE/tool-error"

# ---- AC6-EDGE: a silent probe success (rc 0, no output) is not an absence ----
echo "AC6-EDGE: silent probe refuses"
RC=$(run --control '/known-token/' --probe '/other-token-missing/' -- awk {} f.txt)
if [[ $RC -eq 2 ]]; then pass "rc=2 on silent probe"; else fail "rc=$RC (expected 2)"; fi
if grep -q 'silent probe' "$REPO/stderr"; then
    pass "stderr names the silent probe"
else
    fail "stderr missing 'silent probe': $(cat "$REPO/stderr")"
fi
assert_no_marker "AC6-EDGE/silent-probe"

# ---- summary ----
TOTAL=$((PASS + FAIL))
echo
if [[ $FAIL -eq 0 ]]; then
    echo "PASS: assert-absent.sh tests ($PASS/$TOTAL)"
    exit 0
else
    echo "FAIL: assert-absent.sh tests ($PASS passed, $FAIL failed of $TOTAL)"
    exit 1
fi
