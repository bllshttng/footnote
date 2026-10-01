#!/usr/bin/env bash
# Tests for the in_review binding guard in hooks/helpers/init-target-state.sh.
#
# A fresh named-node bootstrap asks the native owner (`fno backlog
# target-binding --env`) which node it may bind, and acts on the verdict:
# continue proceeds, adopt stamps target_adopted_pr, refused (exit 1) and
# forked (exit 3) exit before any manifest. The hook resolves the node and
# forwards its input; it carries no classifier of its own. Free text and a
# resume never ask. A verdict `fno do target init` already read is trusted
# instead of asking twice.
#
# The verdict itself is native and covered by the crate's unit tests, so the
# stub here answers the receipt from STUB_BIND and records the argv the hook
# sent. The stub is SELF-CONTAINED: every other verb is a benign success, so
# the proceed path needs no real fno (CI smoke has none on PATH).

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

NODE="ab-12345678"
TMP_BASE="$(mktemp -d -t target-init-review-XXXXXX)"
trap 'rm -rf "$TMP_BASE"' EXIT

# ── Self-contained fno stub ─────────────────────────────────────────────────
STUB_BIN="$TMP_BASE/bin"
mkdir -p "$STUB_BIN"
cat > "$STUB_BIN/fno" <<'STUB'
#!/usr/bin/env bash
if [[ "${1:-} ${2:-}" == "backlog target-binding" ]]; then
    [[ -n "${STUB_MARKER:-}" ]] && printf '%s\n' "$*" > "$STUB_MARKER"
    case "${STUB_BIND:-continue}" in
        continue) printf 'verdict=continue\nreason=not_delivered\n'; exit 0 ;;
        adopt)
            echo "target binding: ADOPTED: re-binding this session on the open PR #${STUB_PR:-}" >&2
            printf 'verdict=adopt\npr=%s\n' "${STUB_PR:-}"; exit 0 ;;
        refused)
            echo "target binding: REFUSED: node ab-12345678 is in_review (PR #${STUB_PR:-})." >&2
            printf 'verdict=refused\nreason=existing_pr\n'; exit 1 ;;
        forked)
            echo "target binding: FORKED: ab-12345678 already has a PR, so this follow-up binds child x-c1d1 (filed)." >&2
            printf 'verdict=forked\neffective_node=x-c1d1\nnext=fno do target start x-c1d1 --no-merge\n'; exit 3 ;;
        *) exit 2 ;;
    esac
fi
if [[ "${1:-} ${2:-}" == "backlog get" ]]; then
    field=""; prev=""
    for a in "$@"; do [[ "$prev" == "--field" ]] && field="$a"; prev="$a"; done
    case "$field" in
        _archived)
            # Presence probe: exit 1 absent, True archived, null live; any
            # other code is "could not read the graph".
            [[ -n "${STUB_ARCHIVED_RC:-}" ]] && exit "$STUB_ARCHIVED_RC"
            printf '%s\n' "${STUB_ARCHIVED:-null}"; exit 0;;
        pr_number) printf '%s\n' "${STUB_PR:-null}"; exit 0;;
    esac
fi
exit 0
STUB
chmod +x "$STUB_BIN/fno"
# init resolves its manifest through `fno-agents state path`; pin the answer to
# the scenario space dir run_init chooses.
cp "$REPO_ROOT/tests/helpers/fno-agents-state-path-stub.sh" "$STUB_BIN/fno-agents"
chmod +x "$STUB_BIN/fno-agents"

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
    # A scratch HOME that is NOT the repo root: the state-root guard exits
    # init when <repo>/.fno IS the state root.
    mkdir -p "$dir/home/.fno"
}

run_init() {
    local cwd="$1"; shift
    local home="$cwd/home"
    mkdir -p "$home/.fno"
    (
        cd "$cwd"
        unset TARGET_START TARGET_INPUT TARGET_PLAN_PATH TARGET_ALLOW_IN_REVIEW \
              TARGET_SIZE TARGET_ADOPTED_PR FNO_TARGET_BINDING STUB_BIND STUB_PR \
              STUB_MARKER STUB_ARCHIVED STUB_ARCHIVED_RC
        env TARGET_START=1 TARGET_SESSION_ID=review-gate-test-session \
            CLAUDE_PLUGIN_ROOT="$REPO_ROOT" HOME="$home" \
            FNO_TEST_SPACE="$cwd/space" \
            PATH="$STUB_BIN:$PATH" "$@" bash "$INIT_SCRIPT" 2>&1
    )
    return $?
}

echo "=== test-init-in-review-gate ==="

# --- refused: a fresh dispatch on a shipped node exits before any manifest --
echo ""
echo "--- refused: in_review node refuses with no state ---"
T="$TMP_BASE/refused"; make_repo "$T"; MK="$T/bind.args"
OUT=$(run_init "$T" TARGET_INPUT="$NODE" STUB_BIND=refused STUB_PR=999 STUB_MARKER="$MK"); EC=$?
[[ $EC -eq 1 ]] && pass "refused: exit 1" || fail "refused: expected exit 1, got $EC. Output: $OUT"
grep -q "REFUSED: node $NODE is in_review (PR #999)" <<<"$OUT" && pass "refused: native message reaches the operator" || fail "refused: message missing. Got: $OUT"
grep -q "Refusing to write state file" <<<"$OUT" && pass "refused: says no state" || fail "refused: no-state line missing. Got: $OUT"
[[ ! -f "$T/space/target-state.md" ]] && pass "refused: no state file written" || fail "refused: target-state.md written despite refusal"
grep -q -- "--env --phase init --node $NODE --input $NODE" "$MK" && pass "refused: hook forwards node, input and phase" || fail "refused: argv wrong: $(cat "$MK" 2>/dev/null)"
! grep -q -- "--allow-in-review" "$MK" && pass "refused: no allowance without the env" || fail "refused: allowance forwarded unasked"

# --- forked: the allowance plus scope binds a child, never this tree -------
echo ""
echo "--- forked: allowance with scope forks a child and writes nothing here ---"
T="$TMP_BASE/forked"; make_repo "$T"; MK="$T/bind.args"
OUT=$(run_init "$T" TARGET_INPUT="$NODE watch-expiry recovery only" TARGET_ALLOW_IN_REVIEW=1 \
      STUB_BIND=forked STUB_MARKER="$MK"); EC=$?
[[ $EC -eq 3 ]] && pass "forked: exit 3" || fail "forked: expected exit 3, got $EC. Output: $OUT"
grep -q "binds child x-c1d1" <<<"$OUT" && pass "forked: names the child" || fail "forked: child missing. Got: $OUT"
[[ ! -f "$T/space/target-state.md" ]] && pass "forked: parent tree holds no manifest" || fail "forked: manifest written in the parent tree"
grep -q -- "--input $NODE watch-expiry recovery only --allow-in-review" "$MK" && pass "forked: scope and allowance reach the native owner" || fail "forked: argv wrong: $(cat "$MK" 2>/dev/null)"

# --- adopt: the open PR's own worktree resumes that PR ---------------------
echo ""
echo "--- adopt: verdict stamps target_adopted_pr ---"
T="$TMP_BASE/adopt"; make_repo "$T"
OUT=$(run_init "$T" TARGET_INPUT="$NODE" STUB_BIND=adopt STUB_PR=4242); EC=$?
[[ $EC -eq 0 ]] && pass "adopt: exit 0" || fail "adopt: expected exit 0, got $EC. Output: $OUT"
grep -q '^target_adopted_pr: 4242' "$T/space/target-state.md" 2>/dev/null && pass "adopt: manifest names the adopted PR" || fail "adopt: target_adopted_pr missing"

# --- an inherited adopted PR is never stamped without an adopt verdict ------
echo ""
echo "--- continue: stray TARGET_ADOPTED_PR is dropped ---"
T="$TMP_BASE/continue"; make_repo "$T"
OUT=$(run_init "$T" TARGET_INPUT="$NODE" STUB_BIND=continue TARGET_ADOPTED_PR=777); EC=$?
[[ $EC -eq 0 ]] && pass "continue: exit 0" || fail "continue: expected exit 0, got $EC. Output: $OUT"
! grep -q '^target_adopted_pr:' "$T/space/target-state.md" 2>/dev/null && pass "continue: no adopted PR stamped" || fail "continue: inherited TARGET_ADOPTED_PR leaked into the manifest"

# --- a verdict init already read is trusted, not asked again ----------------
echo ""
echo "--- passed verdict: the hook does not ask twice ---"
T="$TMP_BASE/passed"; make_repo "$T"; MK="$T/bind.args"
OUT=$(run_init "$T" TARGET_INPUT="$NODE" FNO_TARGET_BINDING=adopt TARGET_ADOPTED_PR=4242 \
      STUB_BIND=refused STUB_MARKER="$MK"); EC=$?
[[ $EC -eq 0 ]] && pass "passed: exit 0" || fail "passed: expected exit 0, got $EC. Output: $OUT"
[[ ! -f "$MK" ]] && pass "passed: native owner not asked again" || fail "passed: hook re-asked the native owner"
grep -q '^target_adopted_pr: 4242' "$T/space/target-state.md" 2>/dev/null && pass "passed: adopted PR rides through" || fail "passed: adopted PR missing"

# --- free text and resume never ask ----------------------------------------
echo ""
echo "--- free text: no node, no binding call ---"
T="$TMP_BASE/free"; make_repo "$T"; MK="$T/bind.args"
OUT=$(run_init "$T" TARGET_INPUT="fix the login bug" STUB_BIND=refused STUB_MARKER="$MK"); EC=$?
[[ $EC -eq 0 ]] && pass "free: exit 0" || fail "free: expected exit 0, got $EC. Output: $OUT"
[[ ! -f "$MK" ]] && pass "free: binding never asked" || fail "free: binding asked for a free-text input"
[[ -f "$T/space/target-state.md" ]] && pass "free: state file written" || fail "free: state file missing"

echo ""
echo "--- resume: valid state present skips the guard ---"
T="$TMP_BASE/resume"; make_repo "$T"; mkdir -p "$T/space"; MK="$T/bind.args"
cat > "$T/space/target-state.md" <<'MANIFEST'
---
session_id: preexisting
input: "ab-12345678"
plan_path: ""
---
# Target Session State
MANIFEST
OUT=$(run_init "$T" TARGET_INPUT="$NODE" STUB_BIND=refused STUB_MARKER="$MK"); EC=$?
[[ $EC -eq 0 ]] && pass "resume: exit 0" || fail "resume: expected exit 0, got $EC. Output: $OUT"
[[ ! -f "$MK" ]] && pass "resume: binding never asked on resume" || fail "resume: binding asked on resume"

# --- an unanswering bridge warns and proceeds, as a missing fno always did --
echo ""
echo "--- bridge failure: warns, does not abort ---"
T="$TMP_BASE/broken"; make_repo "$T"
OUT=$(run_init "$T" TARGET_INPUT="$NODE" STUB_BIND=broken); EC=$?
[[ $EC -eq 0 ]] && pass "broken: exit 0" || fail "broken: expected exit 0, got $EC. Output: $OUT"
grep -q "binding guard is not running for $NODE" <<<"$OUT" && pass "broken: names the gap" || fail "broken: no warning. Got: $OUT"

# A lagging fno-agents gives no verdict, but a node that has a PR still refuses.
T="$TMP_BASE/broken-pr"; make_repo "$T"
OUT=$(run_init "$T" TARGET_INPUT="$NODE" STUB_BIND=broken STUB_PR=999); EC=$?
[[ $EC -eq 1 ]] && pass "broken-pr: refuses" || fail "broken-pr: expected exit 1, got $EC. Output: $OUT"
grep -q "has PR #999 and fno backlog target-binding gave no verdict" <<<"$OUT" && pass "broken-pr: names the PR and the fix" || fail "broken-pr: message missing. Got: $OUT"
[[ ! -f "$T/space/target-state.md" ]] && pass "broken-pr: no state file" || fail "broken-pr: manifest written for a node with a PR"

# --- an UNREADABLE graph on the presence probe fails open LOUDLY -------------
echo ""
echo "--- probe failure: warns, and does not abort init ---"
T="$TMP_BASE/probe-fail"; make_repo "$T"
OUT=$(run_init "$T" TARGET_INPUT="$NODE" STUB_ARCHIVED_RC=3 STUB_BIND=refused); EC=$?
[[ $EC -eq 0 ]] && pass "probe-fail: init still exits 0 (fails open under set -e)" || fail "probe-fail: expected exit 0, got $EC. Output: $OUT"
grep -q "could not resolve .* against the graph" <<<"$OUT" && pass "probe-fail: names the unresolvable token" || fail "probe-fail: no warning emitted. Got: $OUT"
grep -q "exit 3" <<<"$OUT" && pass "probe-fail: reports the probe exit code" || fail "probe-fail: exit code not named. Got: $OUT"

echo ""
echo "--- absent node: no probe-failure warning ---"
T="$TMP_BASE/probe-absent"; make_repo "$T"
OUT=$(run_init "$T" TARGET_INPUT="$NODE" STUB_ARCHIVED_RC=1); EC=$?
[[ $EC -eq 0 ]] && pass "probe-absent: exit 0" || fail "probe-absent: expected exit 0, got $EC"
! grep -q "could not resolve .* against the graph" <<<"$OUT" && pass "probe-absent: exit 1 stays quiet" || fail "probe-absent: absent node warned like a read failure. Got: $OUT"

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]]
