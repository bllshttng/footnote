#!/usr/bin/env bash
# End-to-end tests for the guarded do-provenance stamp (x-0469).
#
# These exercise the REAL fno-agents binary against synthetic manifests, with a
# `fno` shim on PATH capturing argv. That is the only way to pin two things the
# unit tests structurally cannot: that finalize passes the manifest's created_at
# as --claimed-at (rather than, say, now), and that each guard's decision
# actually reaches the subprocess boundary.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INIT_SCRIPT="$REPO_ROOT/hooks/helpers/init-target-state.sh"
BIN="$REPO_ROOT/crates/fno-agents/target/debug/fno-agents"

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; ((PASS++)); }
fail() { echo "  FAIL: $1"; ((FAIL++)); }

# `--` before the pattern: most needles here are argv fragments starting with
# `--`, which grep would otherwise parse as its own flags.
assert_contains() {
    local desc="$1" needle="$2" haystack="$3"
    if printf '%s\n' "$haystack" | grep -qF -- "$needle"; then pass "$desc"
    else fail "$desc (missing '$needle' in: $haystack)"; fi
}
assert_not_contains() {
    local desc="$1" needle="$2" haystack="$3"
    if printf '%s\n' "$haystack" | grep -qF -- "$needle"; then fail "$desc (unexpected '$needle')"
    else pass "$desc"; fi
}

if [[ ! -x "$BIN" ]]; then
    echo "SKIP: $BIN not built (run: cargo build --manifest-path crates/fno-agents/Cargo.toml)"
    exit 0
fi

# ---- fixture ----
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/bin" "$T/repo/.fno"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$*" >> "$CAPTURE"\n' > "$T/bin/fno"
chmod +x "$T/bin/fno"
export PATH="$T/bin:$PATH" CAPTURE="$T/capture.txt"

cd "$T/repo" || exit 1
git init -q . && git config user.email t@t && git config user.name t
# Commits change a file: work evidence requires a content change, so an
# --allow-empty fixture would exercise a case the guard deliberately rejects.
commit_at() {  # msg author_epoch committer_epoch
    local name
    name=$(printf '%s' "$1" | tr -cd '[:alnum:]')
    printf '%s\n' "$1" > "$name.txt"
    git add -A
    GIT_AUTHOR_DATE="@$2 +0000" GIT_COMMITTER_DATE="@$3 +0000" \
        git commit -q -m "$1"
}
commit_at base 1000 1000
BASE=$(git rev-parse HEAD)
printf -- '---\nstatus: ready\nclaims: ab-e2e0001\n---\n' > "$T/repo/plan.md"

# The manifest is deliberately NOT named target-state.md: finalize takes any
# --state path, and the immutable-manifest guard rejects writes to that name.
CREATED="2026-07-20T00:00:00Z"
M="$T/repo/.fno/manifest-fixture.md"
printf -- '---\nfno_id: run-1\ncreated_at: %s\ninitial_head: %s\nharness_session_id: SESSION-LIVE\nplan_path: "%s"\n---\n# Target Session State\ngraph_node_id: ab-e2e0001\n' \
    "$CREATED" "$BASE" "$T/repo/plan.md" > "$M"

run_finalize() {  # reason -> stderr lines mentioning the do stamp
    "$BIN" finalize --state "$M" --cwd "$T/repo" --reason "$1" 2>&1 | grep -i 'do stamp' || true
}

# ---- AC4-ERR: respawn onto an already-green PR (empty range) ----
echo ""
echo "test_ac4_err_respawn_onto_green_pr_never_stamps"
: > "$CAPTURE"
OUT=$(run_finalize DonePRGreen)
assert_contains "AC4-ERR: skips on no authored work" "G4" "$OUT"
assert_not_contains "AC4-ERR: no session add call" "session add" "$(cat "$CAPTURE")"

# ---- AC4b-ERR: rebase-only successor (author date preserved, committer rewritten) ----
echo ""
echo "test_ac4b_err_rebase_only_successor_never_stamps"
: > "$CAPTURE"
commit_at "predecessor work, replayed by a rebase" 500 9999999999
OUT=$(run_finalize DonePRGreen)
assert_contains "AC4b-ERR: skips a rebase-only successor" "G4" "$OUT"
assert_not_contains "AC4b-ERR: no session add call" "session add" "$(cat "$CAPTURE")"

# ---- merge-payload: upstream commits are not this session's work ----
echo ""
echo "test_upstream_merge_payload_is_not_work_evidence"
: > "$CAPTURE"
git checkout -q -b upstream
commit_at "someone else's work" 1800000000 1800000000
git checkout -q -
git merge -q --no-ff upstream -m "merge upstream" 2>/dev/null
OUT=$(run_finalize DonePRGreen)
assert_contains "merge payload: skips on a merge-only contribution" "G4" "$OUT"
assert_not_contains "merge payload: no session add call" "session add" "$(cat "$CAPTURE")"

# ---- AC3-ERR: planner terminals never stamp, and say which gate ----
echo ""
echo "test_ac3_err_planner_terminals_never_stamp"
for reason in Budget NoProgress Interrupted DoneAdvisory; do
    : > "$CAPTURE"
    OUT=$(run_finalize "$reason")
    assert_contains "AC3-ERR: $reason names the reason gate" "G1" "$OUT"
    assert_not_contains "AC3-ERR: $reason writes nothing" "session add" "$(cat "$CAPTURE")"
done

# ---- AC1-HP: real authored work stamps once, carrying created_at ----
echo ""
echo "test_ac1_hp_authored_work_stamps_with_claimed_at"
: > "$CAPTURE"
commit_at "my own work" 1800000001 1800000001
OUT=$(run_finalize DonePRGreen)
CAP=$(cat "$CAPTURE")
assert_not_contains "AC1-HP: no guard skip" "skipped" "$OUT"
assert_contains "AC1-HP: stamps the node with phase do" "session add ab-e2e0001 --phase do" "$CAP"
assert_contains "AC1-HP: passes the identity guard" "--require-session SESSION-LIVE" "$CAP"
assert_contains "AC1-HP: passes the plan guard" "--guard-plan $T/repo/plan.md" "$CAP"
# The binding a unit test cannot reach: claimed_at is the manifest's created_at,
# not the finalize instant.
assert_contains "AC1-HP: claimed_at is the manifest created_at" "--claimed-at $CREATED" "$CAP"
if [[ "$(grep -c 'session add' <<< "$CAP")" == "1" ]]; then
    pass "AC1-HP: exactly one stamp call"
else
    fail "AC1-HP: expected exactly one stamp call, got: $CAP"
fi

# ---- DoneAwaitingMerge after a prior non-ship finalize still stamps ----
# DoneAwaitingMerge is not in SHIP_REASONS, so a session that hit Budget first
# would early-return before the always-run tail and silently lose its stamp.
echo ""
echo "test_awaiting_merge_after_a_nonship_finalize_still_stamps"
: > "$CAPTURE"
M3="$T/repo/.fno/manifest-resumed.md"
printf -- '---\nfno_id: run-3\ncreated_at: %s\ninitial_head: %s\nharness_session_id: SESSION-LIVE\nplan_path: "%s"\n---\n# Target Session State\ngraph_node_id: ab-e2e0001\n' \
    "$CREATED" "$BASE" "$T/repo/plan.md" > "$M3"
"$BIN" finalize --state "$M3" --cwd "$T/repo" --reason Budget >/dev/null 2>&1
: > "$CAPTURE"
"$BIN" finalize --state "$M3" --cwd "$T/repo" --reason DoneAwaitingMerge >/dev/null 2>&1
assert_contains "resumed: DoneAwaitingMerge still stamps after a Budget fire" \
    "session add ab-e2e0001 --phase do" "$(cat "$CAPTURE")"

# ---- AC7-EDGE: a legacy manifest without initial_head fails closed ----
echo ""
echo "test_ac7_edge_legacy_manifest_fails_closed"
: > "$CAPTURE"
LEGACY="$T/repo/.fno/legacy-fixture.md"
printf -- '---\nfno_id: run-2\ncreated_at: %s\nharness_session_id: SESSION-LIVE\n---\n# Target Session State\ngraph_node_id: ab-e2e0001\n' \
    "$CREATED" > "$LEGACY"
OUT=$("$BIN" finalize --state "$LEGACY" --cwd "$T/repo" --reason DonePRGreen 2>&1 | grep -i 'do stamp' || true)
assert_contains "AC7-EDGE: skips a manifest with no initial_head" "G4" "$OUT"
assert_not_contains "AC7-EDGE: writes nothing" "session add" "$(cat "$CAPTURE")"

# ---- producer side: init writes a parseable initial_head ----
echo ""
echo "test_init_writes_a_parseable_initial_head"
P="$T/proj"
mkdir -p "$P/.fno" "$P/.home"
git -C "$P" init -q 2>/dev/null
git -C "$P" config user.email t@t && git -C "$P" config user.name t
git -C "$P" commit -q --allow-empty -m base
(cd "$P" && HOME="$P/.home" TARGET_START=1 TARGET_INPUT="x" \
    TARGET_LOCATION_OK=main-acknowledged bash "$INIT_SCRIPT") >/dev/null 2>&1
STATE=$(cat "$P/.fno/target-state.md" 2>/dev/null || echo "")
EXPECTED_HEAD=$(git -C "$P" rev-parse HEAD)
assert_contains "init: writes the current HEAD" "initial_head: $EXPECTED_HEAD" "$STATE"

# A repo with no commits: `git rev-parse HEAD` prints "HEAD" on stdout AND fails,
# so a naive `|| echo null` captures BOTH and emits two YAML lines, corrupting
# the whole manifest rather than just this field.
echo ""
echo "test_init_on_a_commitless_repo_writes_a_single_null"
E="$T/empty"
mkdir -p "$E/.fno" "$E/.home"
git -C "$E" init -q 2>/dev/null
git -C "$E" config user.email t@t && git -C "$E" config user.name t
(cd "$E" && HOME="$E/.home" TARGET_START=1 TARGET_INPUT="x" \
    TARGET_LOCATION_OK=main-acknowledged bash "$INIT_SCRIPT") >/dev/null 2>&1
ESTATE=$(cat "$E/.fno/target-state.md" 2>/dev/null || echo "")
if [[ -n "$ESTATE" ]]; then
    assert_contains "init: commitless repo writes null" "initial_head: null" "$ESTATE"
    if grep -qxF 'null' <<< "$ESTATE"; then
        fail "init: commitless repo emitted a bare 'null' line (manifest YAML corrupt)"
    else
        pass "init: no stray bare 'null' line"
    fi
    if command -v python3 >/dev/null 2>&1; then
        if python3 -c "
import sys, yaml
t = sys.stdin.read().split('---')
yaml.safe_load(t[1])
" <<< "$ESTATE" 2>/dev/null; then
            pass "init: commitless-repo frontmatter parses as YAML"
        else
            fail "init: commitless-repo frontmatter is not parseable YAML"
        fi
    fi
else
    pass "init: commitless repo refused to init (acceptable; nothing to corrupt)"
fi

echo ""
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"

[[ $FAIL -eq 0 ]]
