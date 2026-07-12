#!/usr/bin/env bash
# Tests for the in_review dispatch guard in hooks/helpers/init-target-state.sh.
#
# A fresh named-node /target dispatch (/target <id>, fno target start <id>,
# fno target init --input <id>) must REFUSE when the node already carries an
# open, unmerged PR (derives _status == in_review) -- unless TARGET_ALLOW_IN_REVIEW=1.
# Resume of an existing session, free-text/plan inputs, and any non-in_review
# or unreadable status must proceed unchanged (fail-open).
#
# in_review is a DERIVED status (open PR), so we cannot fabricate it in a temp
# graph. Instead we stub `fno` on PATH to answer the `backlog get --field
# _status|pr_number` probe from STUB_STATUS/STUB_PR and delegate everything else
# to the real binary. A marker file proves the guard never probed (free-text,
# resume).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INIT_SCRIPT="$REPO_ROOT/hooks/helpers/init-target-state.sh"
REAL_FNO="$(command -v fno || true)"

if [[ ! -f "$INIT_SCRIPT" ]]; then
    echo "FAIL: init-target-state.sh not found at $INIT_SCRIPT" >&2
    exit 1
fi
if [[ -z "$REAL_FNO" ]]; then
    echo "FAIL: real fno not found on PATH (needed for stub delegation)" >&2
    exit 1
fi

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

# The proceed path acquires a real claim for the fake node into ~/.fno/claims;
# release it on exit so a test run leaves no footprint in real state.
NODE="ab-12345678"
cleanup() {
    rm -rf "$TMP_BASE"
    FNO_CLAIMS_ROOT="$HOME" "$REAL_FNO" claim release "node:$NODE" >/dev/null 2>&1 || true
}
TMP_BASE="$(mktemp -d -t target-init-review-XXXXXX)"
trap cleanup EXIT

# ── fno stub: intercept only `backlog get ... --field _status|pr_number` ─────
STUB_BIN="$TMP_BASE/bin"
mkdir -p "$STUB_BIN"
cat > "$STUB_BIN/fno" <<STUB
#!/usr/bin/env bash
if [[ "\${1:-}" == "backlog" && "\${2:-}" == "get" ]]; then
    field=""; prev=""
    for a in "\$@"; do [[ "\$prev" == "--field" ]] && field="\$a"; prev="\$a"; done
    case "\$field" in
        _status)
            [[ -n "\${STUB_MARKER:-}" ]] && : > "\$STUB_MARKER"
            [[ "\${STUB_STATUS:-}" == "__fail__" ]] && exit 1
            printf '%s\n' "\${STUB_STATUS:-}"; exit 0;;
        pr_number)
            printf '%s\n' "\${STUB_PR:-}"; exit 0;;
    esac
fi
exec "$REAL_FNO" "\$@"
STUB
chmod +x "$STUB_BIN/fno"

make_repo() {
    local dir="$1"
    mkdir -p "$dir"
    (
        cd "$dir"
        git init -q -b feature/in-review-test 2>/dev/null || {
            git init -q; git checkout -q -b feature/in-review-test
        }
        git config user.email "test@test.com"
        git config user.name "Test"
        echo "# test" > README.md
        git add README.md
        git commit -q -m "init"
    )
}

# Run init isolated. cwd is a per-scenario worktree-like repo on a feature
# branch (so the location gate never fires). Real HOME is kept so the delegated
# real fno works without re-provisioning; the guard uses the ab- id path which
# never reads the graph, so real state is untouched (the transient claim is
# released in cleanup).
run_init() {
    local cwd="$1"; shift
    (
        cd "$cwd"
        unset TARGET_START TARGET_INPUT TARGET_PLAN_PATH TARGET_ALLOW_IN_REVIEW \
              TARGET_SIZE STUB_STATUS STUB_PR STUB_MARKER
        env TARGET_START=1 CLAUDE_PLUGIN_ROOT="$REPO_ROOT" \
            PATH="$STUB_BIN:$PATH" "$@" bash "$INIT_SCRIPT" 2>&1
    )
    return $?
}

echo "=== test-init-in-review-gate (x-2dc5) ==="

# --- AC1-HP: fresh named-node dispatch on in_review node REFUSED ------------
echo ""
echo "--- AC1-HP: in_review node refuses ---"
T="$TMP_BASE/ac1"; make_repo "$T"
OUT=$(run_init "$T" TARGET_INPUT="$NODE" STUB_STATUS=in_review STUB_PR=999); EC=$?
[[ $EC -ne 0 ]] && pass "AC1-HP: non-zero exit ($EC)" || fail "AC1-HP: expected non-zero exit, got 0"
echo "$OUT" | grep -q "REFUSED: node $NODE is in_review" && pass "AC1-HP: refusal names node" || fail "AC1-HP: refusal message missing. Got: $OUT"
echo "$OUT" | grep -q "#999" && pass "AC1-HP: refusal names open PR number" || fail "AC1-HP: PR number missing. Got: $OUT"
echo "$OUT" | grep -q "/pr check" && pass "AC1-HP: refusal points at /pr check" || fail "AC1-HP: /pr check hint missing. Got: $OUT"
echo "$OUT" | grep -q "TARGET_ALLOW_IN_REVIEW=1" && pass "AC1-HP: refusal documents override env" || fail "AC1-HP: override env hint missing. Got: $OUT"
[[ ! -f "$T/.fno/target-state.md" ]] && pass "AC1-HP: no state file written" || fail "AC1-HP: target-state.md written despite refusal"

# --- AC2-HP: override forces a fresh run ------------------------------------
echo ""
echo "--- AC2-HP: TARGET_ALLOW_IN_REVIEW=1 proceeds ---"
T="$TMP_BASE/ac2"; make_repo "$T"
OUT=$(run_init "$T" TARGET_INPUT="$NODE" STUB_STATUS=in_review STUB_PR=999 TARGET_ALLOW_IN_REVIEW=1); EC=$?
[[ $EC -eq 0 ]] && pass "AC2-HP: exit 0 with override" || fail "AC2-HP: expected exit 0, got $EC. Output: $OUT"
! echo "$OUT" | grep -q "REFUSED" && pass "AC2-HP: no refusal under override" || fail "AC2-HP: unexpected refusal under override. Got: $OUT"
[[ -f "$T/.fno/target-state.md" ]] && pass "AC2-HP: state file written under override" || fail "AC2-HP: state file missing under override"

# --- AC3-ERR: free-text input is never guarded (no probe) -------------------
echo ""
echo "--- AC3-ERR: free-text input skips the guard ---"
T="$TMP_BASE/ac3"; make_repo "$T"; MK="$T/probed.marker"
OUT=$(run_init "$T" TARGET_INPUT="fix the login bug" STUB_STATUS=in_review STUB_MARKER="$MK"); EC=$?
[[ $EC -eq 0 ]] && pass "AC3-ERR: exit 0 on free-text" || fail "AC3-ERR: expected exit 0, got $EC. Output: $OUT"
! echo "$OUT" | grep -q "REFUSED" && pass "AC3-ERR: no refusal on free-text" || fail "AC3-ERR: unexpected refusal on free-text. Got: $OUT"
[[ ! -f "$MK" ]] && pass "AC3-ERR: guard never probed backlog get" || fail "AC3-ERR: guard probed status for a free-text input"
[[ -f "$T/.fno/target-state.md" ]] && pass "AC3-ERR: state file written on free-text" || fail "AC3-ERR: state file missing on free-text"

# --- AC4-EDGE: non-in_review status proceeds -------------------------------
echo ""
echo "--- AC4-EDGE: ready status proceeds ---"
T="$TMP_BASE/ac4a"; make_repo "$T"
OUT=$(run_init "$T" TARGET_INPUT="$NODE" STUB_STATUS=ready); EC=$?
[[ $EC -eq 0 ]] && pass "AC4-EDGE(ready): exit 0" || fail "AC4-EDGE(ready): expected exit 0, got $EC. Output: $OUT"
! echo "$OUT" | grep -q "REFUSED" && pass "AC4-EDGE(ready): no refusal" || fail "AC4-EDGE(ready): unexpected refusal. Got: $OUT"

# --- AC4-EDGE: backlog get failure fails open ------------------------------
echo ""
echo "--- AC4-EDGE: backlog get failure fails open ---"
T="$TMP_BASE/ac4b"; make_repo "$T"
OUT=$(run_init "$T" TARGET_INPUT="$NODE" STUB_STATUS=__fail__); EC=$?
[[ $EC -eq 0 ]] && pass "AC4-EDGE(fail): exit 0 (fail-open)" || fail "AC4-EDGE(fail): expected exit 0, got $EC. Output: $OUT"
! echo "$OUT" | grep -q "REFUSED" && pass "AC4-EDGE(fail): no refusal on probe failure" || fail "AC4-EDGE(fail): unexpected refusal on probe failure. Got: $OUT"

# --- AC5-FR: refusal leaves no stub; re-run with override bootstraps clean --
echo ""
echo "--- AC5-FR: refuse then override bootstraps clean ---"
T="$TMP_BASE/ac5"; make_repo "$T"
run_init "$T" TARGET_INPUT="$NODE" STUB_STATUS=in_review STUB_PR=999 >/dev/null 2>&1
[[ ! -f "$T/.fno/target-state.md" ]] && pass "AC5-FR: refusal left no state" || fail "AC5-FR: refusal left a state file"
OUT=$(run_init "$T" TARGET_INPUT="$NODE" STUB_STATUS=in_review STUB_PR=999 TARGET_ALLOW_IN_REVIEW=1); EC=$?
[[ $EC -eq 0 && -f "$T/.fno/target-state.md" ]] && pass "AC5-FR: override re-run bootstraps clean" || fail "AC5-FR: override re-run failed (exit $EC). Output: $OUT"

# --- AC5-FR: resume (valid state present) never fires the guard ------------
echo ""
echo "--- AC5-FR: resume skips the guard entirely ---"
T="$TMP_BASE/ac5b"; make_repo "$T"; mkdir -p "$T/.fno"; MK="$T/probed.marker"
cat > "$T/.fno/target-state.md" <<'MANIFEST'
---
session_id: preexisting
input: "ab-12345678"
plan_path: ""
---
# Target Session State
MANIFEST
OUT=$(run_init "$T" TARGET_INPUT="$NODE" STUB_STATUS=in_review STUB_MARKER="$MK"); EC=$?
[[ $EC -eq 0 ]] && pass "AC5-FR(resume): exit 0" || fail "AC5-FR(resume): expected exit 0, got $EC. Output: $OUT"
! echo "$OUT" | grep -q "REFUSED" && pass "AC5-FR(resume): no refusal on resume" || fail "AC5-FR(resume): guard fired on resume. Got: $OUT"
[[ ! -f "$MK" ]] && pass "AC5-FR(resume): guard never probed on resume" || fail "AC5-FR(resume): guard probed status on resume"

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]]
