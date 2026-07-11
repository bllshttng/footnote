#!/usr/bin/env bash
# Tests for auto_merge getters in scripts/lib/config.sh
# Run from any directory - uses absolute paths
# Uses temp HOME directories so real ~/.fno/config.toml is never touched.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PASS=0
FAIL=0

# Source the config library (scripts/tests/ -> scripts/lib/)
source "$SCRIPT_DIR/../lib/config.sh"

# ---- Test helpers ----

pass() { echo "  PASS: $1"; ((PASS++)); }
fail() { echo "  FAIL: $1"; ((FAIL++)); }

assert_eq() {
    local desc="$1" expected="$2" actual="$3"
    if [[ "$actual" == "$expected" ]]; then
        pass "$desc"
    else
        fail "$desc (expected='$expected', got='$actual')"
    fi
}

assert_exit_0() {
    local desc="$1"
    shift
    if "$@" 2>/dev/null; then
        pass "$desc"
    else
        fail "$desc (expected exit 0, got non-zero)"
    fi
}

assert_exit_1() {
    local desc="$1"
    shift
    if ! "$@" 2>/dev/null; then
        pass "$desc"
    else
        fail "$desc (expected exit 1, got 0)"
    fi
}

# ---- test_enabled_default_false ----
# No settings anywhere → get_auto_merge_enabled returns "false"

echo ""
echo "test_enabled_default_false"
TMPDIR_T=$(mktemp -d)
trap "rm -rf '$TMPDIR_T'" EXIT
LOCAL_SETTINGS="$TMPDIR_T/nonexistent/config.toml"
GLOBAL_SETTINGS="$TMPDIR_T/nonexistent2/config.toml"
LEGACY_CONFIG="$TMPDIR_T/nonexistent3/config.yaml"
CLAUDE_SETTINGS="$TMPDIR_T/nonexistent4.json"
CLAUDE_SETTINGS_LOCAL="$TMPDIR_T/nonexistent5.json"
result=$(get_auto_merge_enabled)
assert_eq "test_enabled_default_false: no settings → false" "false" "$result"

# ---- test_enabled_local_true ----
# Only local settings says enabled: true → returns "true"

echo ""
echo "test_enabled_local_true"
mkdir -p "$TMPDIR_T/local_true/.fno"
cat > "$TMPDIR_T/local_true/.fno/config.toml" <<'YAML'
[auto_merge]
enabled = true
YAML
LOCAL_SETTINGS="$TMPDIR_T/local_true/.fno/config.toml"
GLOBAL_SETTINGS="$TMPDIR_T/nonexistent2/config.toml"
LEGACY_CONFIG="$TMPDIR_T/nonexistent3/config.yaml"
CLAUDE_SETTINGS="$TMPDIR_T/nonexistent4.json"
CLAUDE_SETTINGS_LOCAL="$TMPDIR_T/nonexistent5.json"
result=$(get_auto_merge_enabled)
assert_eq "test_enabled_local_true: local enabled: true → true" "true" "$result"

# ---- test_local_overrides_global ----
# Local says false, global says true → local wins → "false"

echo ""
echo "test_local_overrides_global"
mkdir -p "$TMPDIR_T/local_false/.fno" "$TMPDIR_T/global_true"
cat > "$TMPDIR_T/local_false/.fno/config.toml" <<'YAML'
[auto_merge]
enabled = false
YAML
cat > "$TMPDIR_T/global_true/config.toml" <<'YAML'
[auto_merge]
enabled = true
YAML
LOCAL_SETTINGS="$TMPDIR_T/local_false/.fno/config.toml"
GLOBAL_SETTINGS="$TMPDIR_T/global_true/config.toml"
LEGACY_CONFIG="$TMPDIR_T/nonexistent3/config.yaml"
CLAUDE_SETTINGS="$TMPDIR_T/nonexistent4.json"
CLAUDE_SETTINGS_LOCAL="$TMPDIR_T/nonexistent5.json"
result=$(get_auto_merge_enabled)
assert_eq "test_local_overrides_global: local false beats global true → false" "false" "$result"

# ---- test_strategy_invalid_falls_back ----
# merge_strategy: octopus (invalid) → stderr warning + stdout "merge"

echo ""
echo "test_strategy_invalid_falls_back"
mkdir -p "$TMPDIR_T/bad_strategy/.fno"
cat > "$TMPDIR_T/bad_strategy/.fno/config.toml" <<'YAML'
[auto_merge]
merge_strategy = "octopus"
YAML
LOCAL_SETTINGS="$TMPDIR_T/bad_strategy/.fno/config.toml"
GLOBAL_SETTINGS="$TMPDIR_T/nonexistent2/config.toml"
LEGACY_CONFIG="$TMPDIR_T/nonexistent3/config.yaml"
CLAUDE_SETTINGS="$TMPDIR_T/nonexistent4.json"
CLAUDE_SETTINGS_LOCAL="$TMPDIR_T/nonexistent5.json"
result=$(get_auto_merge_strategy 2>/dev/null)
assert_eq "test_strategy_invalid_falls_back: invalid value → merge" "merge" "$result"
# Also verify the warning goes to stderr
stderr_out=$(get_auto_merge_strategy 2>&1 >/dev/null)
if echo "$stderr_out" | grep -q "invalid merge_strategy"; then
    pass "test_strategy_invalid_falls_back: warning emitted to stderr"
else
    fail "test_strategy_invalid_falls_back: expected stderr warning about invalid merge_strategy"
fi

# ---- test_conflict_resolution_default_opus ----
# No setting → get_auto_merge_conflict_resolution returns "opus"

echo ""
echo "test_conflict_resolution_default_opus"
LOCAL_SETTINGS="$TMPDIR_T/nonexistent/config.toml"
GLOBAL_SETTINGS="$TMPDIR_T/nonexistent2/config.toml"
LEGACY_CONFIG="$TMPDIR_T/nonexistent3/config.yaml"
CLAUDE_SETTINGS="$TMPDIR_T/nonexistent4.json"
CLAUDE_SETTINGS_LOCAL="$TMPDIR_T/nonexistent5.json"
result=$(get_auto_merge_conflict_resolution)
assert_eq "test_conflict_resolution_default_opus: no setting → opus" "opus" "$result"

# The who-may-merge gate (is_auto_merge_allowed_for / allowed_invokers) was
# removed (x-04ab): auto-merge is gated by get_auto_merge_enabled alone, which
# is covered by the enabled/default cases above.

# ---- Summary ----

echo ""
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"

if [[ $FAIL -gt 0 ]]; then
    exit 1
fi
exit 0
