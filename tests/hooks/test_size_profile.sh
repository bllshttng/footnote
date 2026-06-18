#!/usr/bin/env bash
#
# Regression test for the size-profile propagation in init-target-state.sh.
#
# Motivation: the inbox msg-b5312b / PR #500 target-loop incident. `/target S`
# wrote `no_external: false, no_docs: false, no_memory: false` in BOTH the
# live flag block AND `skip_flags_initial`, so the drift detector blocked any
# retroactive correction by the skill body. Required gates that don't apply
# to a Build+PR-only task could not be legitimately satisfied, and the stop
# hook bounced between rejecting LLM-authored BLOCKED and re-prompting
# /target --resume.
#
# Invariants this test pins:
#   1. TARGET_SIZE=S writes the minimal-ceremony profile (no_external,
#      no_docs, no_memory, no_goals, no_browser, no_how_to all true).
#   2. TARGET_SIZE=L writes the maximal profile (all skip flags false).
#   3. TARGET_SIZE unset preserves the legacy M-shaped defaults so existing
#      consumers see no behavior change without explicit opt-in.
#   4. Per-flag TARGET_NO_* env overrides win over the size-profile default.
#   5. For every size, the live block and skip_flags_initial snapshot share
#      identical values for every flag (drift impossible by construction).

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
INIT_SCRIPT="$REPO_ROOT/hooks/helpers/init-target-state.sh"
[[ -x "$INIT_SCRIPT" ]] || { echo "init script not executable: $INIT_SCRIPT" >&2; exit 1; }

fail_count=0
pass() { printf '[size-profile] PASS: %s\n' "$*"; }
fail() { printf '[size-profile] FAIL: %s\n' "$*" >&2; fail_count=$((fail_count + 1)); }

# Run init in an isolated temp project and return the state-file path.
run_init() {
    local size_env="$1"; shift  # extra "NAME=VAL" env assignments
    local tmp
    tmp=$(mktemp -d)
    (
        cd "$tmp"
        git init -q >/dev/null 2>&1
        mkdir -p .fno
        env -i \
            HOME="$HOME" \
            PATH="$PATH" \
            TARGET_START=1 \
            TARGET_INPUT="test" \
            ${size_env:+TARGET_SIZE="$size_env"} \
            "$@" \
            bash "$INIT_SCRIPT" >/dev/null 2>&1
    )
    echo "$tmp/.fno/target-state.md"
}

# Extract live flag value (line outside the skip_flags_initial block).
flag_live() {
    local file="$1" key="$2"
    awk -v key="$key" '
        /^skip_flags_initial:/ { in_snap=1; next }
        in_snap && /^# Completion gates/ { in_snap=0 }
        !in_snap {
            n = length(key) + 1
            if (substr($0, 1, n) == key ":") {
                rest = substr($0, n + 1)
                sub(/^[[:space:]]+/, "", rest)
                sub(/[[:space:]]+$/, "", rest)
                print rest
                exit
            }
        }
    ' "$file"
}

# Assert the live flag matches. The skip_flags_initial snapshot block was
# REMOVED by the control-plane collapse wedge (ab-d0337fbc): the manifest is
# immutable, so the flags ARE the snapshot and the drift detector is gone.
assert_flag() {
    local file="$1" key="$2" expected="$3" label="$4"
    local live
    live=$(flag_live "$file" "$key")
    if [[ "$live" != "$expected" ]]; then
        fail "$label: live $key=$live, expected $expected"
        return 1
    fi
    return 0
}

# ── Scenario 1: TARGET_SIZE=S minimal-ceremony profile ──────────────────
state=$(run_init S)
[[ -f "$state" ]] || { fail "S: state file not written"; exit 1; }
ok=1
for k in no_external no_docs no_memory no_goals no_browser no_how_to no_verify no_clean; do
    assert_flag "$state" "$k" "true" "S" || ok=0
done
assert_flag "$state" "no_ship" "false" "S" || ok=0
assert_flag "$state" "has_ui"  "false" "S" || ok=0
(( ok )) && pass "S: minimal-ceremony profile applied (live + snapshot in lockstep)"
rm -rf "$(dirname "$(dirname "$state")")"

# ── Scenario 2: TARGET_SIZE=L maximal-ceremony profile ──────────────────
state=$(run_init L)
ok=1
for k in no_external no_docs no_memory no_goals no_browser no_how_to no_verify no_clean no_ship has_ui; do
    assert_flag "$state" "$k" "false" "L" || ok=0
done
(( ok )) && pass "L: maximal-ceremony profile applied (every flag false in live + snapshot)"
rm -rf "$(dirname "$(dirname "$state")")"

# ── Scenario 3: TARGET_SIZE unset preserves legacy M defaults ───────────
state=$(run_init "")
ok=1
assert_flag "$state" "no_external" "false" "unset" || ok=0
assert_flag "$state" "no_docs"     "false" "unset" || ok=0
assert_flag "$state" "no_memory"   "false" "unset" || ok=0
assert_flag "$state" "no_verify"   "true"  "unset" || ok=0
assert_flag "$state" "no_clean"    "true"  "unset" || ok=0
(( ok )) && pass "unset: legacy M-shaped defaults preserved for non-opted-in consumers"
rm -rf "$(dirname "$(dirname "$state")")"

# ── Scenario 4: per-flag TARGET_NO_* override wins over size default ────
state=$(run_init S TARGET_NO_DOCS=false TARGET_NO_EXTERNAL=)
# S's default for no_docs is true; explicit TARGET_NO_DOCS=false flips it.
# Empty TARGET_NO_EXTERNAL keeps the profile default (true).
ok=1
assert_flag "$state" "no_docs"     "false" "S+override" || ok=0
assert_flag "$state" "no_external" "true"  "S+override" || ok=0
(( ok )) && pass "S + TARGET_NO_DOCS=false: per-flag override wins, others keep profile"
rm -rf "$(dirname "$(dirname "$state")")"

# ── Scenario 4b: auto-merge forces external review on ─────────────────
# Motivation: a user-reported wasted PR - `/target S x auto-merge` would
# merge the PR immediately because S sets `no_external: true` via profile
# and the Phase 8a gate accepts `external_review_passed: skipped` as a
# green light. Auto-merge semantically means "merge after review"; the
# init script now forces `no_external: false` whenever
# auto_merge_approved is true, regardless of size profile.
state=$(run_init S TARGET_AUTO_MERGE=1)
ok=1
assert_flag "$state" "no_external" "false" "S+auto-merge" || ok=0
# Other S defaults must still apply - only no_external is overridden.
assert_flag "$state" "no_docs"     "true"  "S+auto-merge" || ok=0
assert_flag "$state" "no_browser"  "true"  "S+auto-merge" || ok=0
# auto_merge_approved must read true (sanity).
amv=$(awk -F': *' '/^auto_merge_approved:/ { print $2; exit }' "$state")
if [[ "$amv" != "true" ]]; then
    fail "S+auto-merge: expected auto_merge_approved=true, got '$amv'"
    ok=0
fi
(( ok )) && pass "S + auto-merge: no_external forced false; other S defaults preserved"
rm -rf "$(dirname "$(dirname "$state")")"

# ── Scenario 4c: auto-merge overrides explicit TARGET_NO_EXTERNAL=1 ─────
# Even an explicit --no-external is overridden when auto-merge is on -
# the combination is contradictory and the safer interpretation wins
# (same precedent as `auto-merge` + `no-merge` -> no-merge wins).
state=$(run_init L TARGET_NO_EXTERNAL=1 TARGET_AUTO_MERGE=1)
ok=1
assert_flag "$state" "no_external" "false" "L+no-external+auto-merge" || ok=0
(( ok )) && pass "auto-merge overrides explicit TARGET_NO_EXTERNAL=1"
rm -rf "$(dirname "$(dirname "$state")")"

# ── Scenario 4d: no auto-merge => S profile is unaffected ──────────────
# Pin the negative path so the override only fires when auto-merge is on.
state=$(run_init S)
ok=1
assert_flag "$state" "no_external" "true" "S (no auto-merge)" || ok=0
(( ok )) && pass "S without auto-merge: no_external stays true (override inert)"
rm -rf "$(dirname "$(dirname "$state")")"

# ── Scenario 5: skip_flags_initial is GONE (ab-d0337fbc) ────────────────
# The manifest is immutable, so the live flags ARE the snapshot; the
# skip_flags_initial block and its drift detector were deleted by the
# control-plane collapse wedge. Assert the block never reappears.
for size in S M L; do
    state=$(run_init "$size")
    if grep -q '^skip_flags_initial:' "$state"; then
        fail "$size: skip_flags_initial must NOT be rendered (removed by ab-d0337fbc)"
    else
        pass "$size: no skip_flags_initial block (immutable manifest)"
    fi
    rm -rf "$(dirname "$(dirname "$state")")"
done

if (( fail_count > 0 )); then
    printf '\n[size-profile] %d FAILED\n' "$fail_count" >&2
    exit 1
fi
printf '[size-profile] all scenarios passed\n'
