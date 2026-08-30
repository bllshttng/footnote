#!/usr/bin/env bash
# The fno-absent hold-shape gate in hooks/helpers/init-target-state.sh
# (x-d884 finding 7).
#
# With fno missing from PATH, TARGET_INPUT shapes that fuzzy.resolve_node
# accepts as node references (canonical <prefix>-<hex>, bare <hex>, slug) must
# REFUSE - the hold state cannot be checked, and "cannot check" must not read
# as "unheld". Shapes that cannot name a node (spaced free text, a single
# word) proceed with the free-text note. A plan path and an existing file also
# refuse (unchanged from before).
#
# The refusal is forced, not inferred: each refusing case asserts the named
# REFUSED line and a non-zero exit.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INIT_SCRIPT="$REPO_ROOT/hooks/helpers/init-target-state.sh"

if [[ ! -f "$INIT_SCRIPT" ]]; then
    echo "FAIL: init-target-state.sh not found at $INIT_SCRIPT" >&2
    exit 1
fi

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP_BASE="$(mktemp -d -t target-init-hold-XXXXXX)"
trap 'rm -rf "$TMP_BASE"' EXIT

make_repo() {
    local dir="$1"
    mkdir -p "$dir"
    (
        cd "$dir"
        git init -q -b feature/hold-shape-test 2>/dev/null || {
            git init -q; git checkout -q -b feature/hold-shape-test
        }
        git config user.email "test@test.com"
        git config user.name "Test"
        echo "# test" > README.md
        git add README.md
        git commit -q -m "init"
    )
}

# Run init with fno ABSENT: PATH carries only the OS dirs, so `command -v fno`
# fails and the script takes the degraded branch under test. HOME is the
# scratch repo so nothing real is touched.
run_no_fno() {
    local cwd="$1"; shift
    (
        cd "$cwd"
        unset TARGET_START TARGET_INPUT TARGET_PLAN_PATH FNO_TARGET_INIT_GATED
        env TARGET_START=1 CLAUDE_PLUGIN_ROOT="$REPO_ROOT" HOME="$cwd" \
            PATH="/usr/bin:/bin" "$@" bash "$INIT_SCRIPT" 2>&1
    )
    return $?
}

expect_refusal() {
    local label="$1" input="$2"
    local T="$TMP_BASE/$3"
    make_repo "$T"
    # shellcheck disable=SC2016 # TARGET_INPUT is read by the script, not us
    local OUT EC
    OUT=$(run_no_fno "$T" TARGET_INPUT="$input"); EC=$?
    if [[ $EC -ne 0 ]] && grep -q "REFUSED: fno absent" <<<"$OUT"; then
        pass "$label refuses with the named reason (exit $EC)"
    else
        fail "$label must refuse. exit=$EC output: $OUT"
    fi
    [[ ! -f "$T/.fno/target-state.md" ]] \
        && pass "$label writes no state file" \
        || fail "$label wrote target-state.md despite refusal"
}

echo "=== test-init-hold-shapes (x-d884 F7) ==="

expect_refusal "canonical node id" "ab-12345678" "canonical"
expect_refusal "bare hex id" "77a0" "barehex"
expect_refusal "bare hex, uppercase" "77A0" "barehex-upper"
expect_refusal "slug reference" "blocked-has-full-push-leg-zero" "slug"
expect_refusal "plan path" "internal/fno/plans/some-plan.md" "planpath"

# An existing file input refuses via the -f arm (pre-existing behavior, pinned).
T="$TMP_BASE/existing-file"; make_repo "$T"; touch "$T/idea.md"
OUT=$(run_no_fno "$T" TARGET_INPUT="$T/idea.md"); EC=$?
if [[ $EC -ne 0 ]] && grep -q "REFUSED: fno absent" <<<"$OUT"; then
    pass "existing file input refuses"
else
    fail "existing file input must refuse. exit=$EC output: $OUT"
fi

# Shapes that cannot name a node proceed with the free-text note.
for words in "fix the login bug" "authentication"; do
    T="$TMP_BASE/free-$(basename "$words" | tr ' ' '_')"; make_repo "$T"
    OUT=$(run_no_fno "$T" TARGET_INPUT="$words"); EC=$?
    if grep -q "proceeding with free-text init" <<<"$OUT" && ! grep -q "REFUSED: fno absent" <<<"$OUT"; then
        pass "free text '$words' proceeds with the note"
    else
        fail "free text '$words' must not hit the hold refusal. exit=$EC output: $OUT"
    fi
done

echo ""
if [[ $FAIL -eq 0 ]]; then
    echo "RESULT: PASS ($PASS assertions)"
    exit 0
fi
echo "RESULT: FAIL ($FAIL failing of $((PASS + FAIL)) assertions)"
exit 1
