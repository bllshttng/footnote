#!/usr/bin/env bash
# Tests for auto_merge fields in init-target-state.sh
# Verifies: auto_merge_enabled, auto_merge_approved, merged_prs,
#           merge_auto_queued, merge_failed, conflicts_resolved fields
#           are written to target-state.md at init time.
# Also verifies TARGET_NO_MERGE=1 forces auto_merge_approved: false.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INIT_SCRIPT="$REPO_ROOT/hooks/helpers/init-target-state.sh"

PASS=0
FAIL=0

pass() { echo "  PASS: $1"; ((PASS++)); }
fail() { echo "  FAIL: $1"; ((FAIL++)); }

assert_contains() {
    local desc="$1" needle="$2" haystack="$3"
    if echo "$haystack" | grep -qF "$needle"; then
        pass "$desc"
    else
        fail "$desc (pattern='$needle' not found in output)"
    fi
}

# ---- setup helper ----
# Creates a temp git repo with optional config.toml content, runs init,
# returns path to created state file via stdout.
setup_repo() {
    local settings_content="$1"
    local T
    T=$(mktemp -d)
    mkdir -p "$T/.fno"
    echo "$settings_content" > "$T/.fno/config.toml"
    git -C "$T" init -q 2>/dev/null
    git -C "$T" config user.email "test@test.com"
    git -C "$T" config user.name "Test"
    echo "$T"
}

run_init_in() {
    local tmpdir="$1"
    shift
    # Run with explicit env vars only - no array expansion to avoid set -u issues
    (
      cd "$tmpdir"
      if [[ $# -gt 0 ]]; then
        env "$@" TARGET_START=1 TARGET_INPUT="test feature" bash "$INIT_SCRIPT"
      else
        TARGET_START=1 TARGET_INPUT="test feature" bash "$INIT_SCRIPT"
      fi
    ) 2>/dev/null
}

# ---- Test 1: AC1-HP fields present when auto_merge disabled (default) ----

echo ""
echo "test_auto_merge_fields_present_disabled_by_default"

T=$(setup_repo "expertise = \"frontend\"")

run_in_result=$(run_init_in "$T")
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "AC1-HP: auto_merge_enabled field present" "auto_merge_enabled:" "$STATE"
assert_contains "AC1-HP: auto_merge_approved field present" "auto_merge_approved:" "$STATE"
assert_contains "AC1-HP: merged_prs array present" "merged_prs: []" "$STATE"
assert_contains "AC1-HP: merge_auto_queued array present" "merge_auto_queued: []" "$STATE"
assert_contains "AC1-HP: merge_failed array present" "merge_failed: []" "$STATE"
assert_contains "AC1-HP: conflicts_resolved array present" "conflicts_resolved: []" "$STATE"
assert_contains "AC1-HP: auto_merge_approved false when disabled" "auto_merge_approved: false" "$STATE"
assert_contains "AC1-HP: auto_merge_enabled false when not set" "auto_merge_enabled: false" "$STATE"

rm -rf "$T"

# ---- Test 2: AC2-HP auto_merge_approved true when enabled + target allowed ----

echo ""
echo "test_auto_merge_approved_true_when_enabled"

T=$(setup_repo "[auto_merge]
enabled = true")

run_init_in "$T"
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "AC2-HP: auto_merge_enabled true when set" "auto_merge_enabled: true" "$STATE"
assert_contains "AC2-HP: auto_merge_approved true when enabled" "auto_merge_approved: true" "$STATE"
assert_contains "AC2-HP: arrays still empty at init" "merged_prs: []" "$STATE"

rm -rf "$T"

# ---- Test 3: AC3-ERR TARGET_NO_MERGE=1 overrides to false ----

echo ""
echo "test_target_no_merge_forces_approved_false"

T=$(setup_repo "[auto_merge]
enabled = true")

run_init_in "$T" "TARGET_NO_MERGE=1"
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "AC3-ERR: auto_merge_approved false with TARGET_NO_MERGE=1" "auto_merge_approved: false" "$STATE"

rm -rf "$T"

# ---- Test 4: AC4-VERIFY all 6 fields are in the YAML frontmatter ----

echo ""
echo "test_fields_are_in_yaml_frontmatter"

T=$(setup_repo "[auto_merge]
enabled = true")

run_init_in "$T"
# Extract frontmatter only (between first --- and second ---)
FRONTMATTER=$(awk '/^---/{n++; if(n==2) exit} n==1{print}' "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "AC4-VERIFY: auto_merge_enabled in frontmatter" "auto_merge_enabled:" "$FRONTMATTER"
assert_contains "AC4-VERIFY: auto_merge_approved in frontmatter" "auto_merge_approved:" "$FRONTMATTER"
assert_contains "AC4-VERIFY: merged_prs in frontmatter" "merged_prs:" "$FRONTMATTER"
assert_contains "AC4-VERIFY: merge_auto_queued in frontmatter" "merge_auto_queued:" "$FRONTMATTER"
assert_contains "AC4-VERIFY: merge_failed in frontmatter" "merge_failed:" "$FRONTMATTER"
assert_contains "AC4-VERIFY: conflicts_resolved in frontmatter" "conflicts_resolved:" "$FRONTMATTER"

rm -rf "$T"

# ---- Summary ----

echo ""
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"

[[ $FAIL -eq 0 ]]
