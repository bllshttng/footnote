#!/usr/bin/env bash
# Tests for config.sh
# Run from any directory - uses absolute paths

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PASS=0
FAIL=0

# Source the config library
source "$SCRIPT_DIR/config.sh"

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

# ---- Setup: temp config dir ----

TMPDIR_TEST=$(mktemp -d)
trap "rm -rf '$TMPDIR_TEST'" EXIT

# ---- AC1: Load config from settings.yaml config: section ----

echo ""
echo "AC1: Load config from settings.yaml config: section"
mkdir -p "$TMPDIR_TEST/ac1/.fno"
cat > "$TMPDIR_TEST/ac1/.fno/settings.yaml" <<'YAML'
work:
  workspaces:
    test:
      projects: []

config:
  expertise: frontend
  max_iterations: 20
YAML
LOCAL_SETTINGS="$TMPDIR_TEST/ac1/.fno/settings.yaml"
GLOBAL_SETTINGS="$TMPDIR_TEST/nonexistent/settings.yaml"
LEGACY_CONFIG="$TMPDIR_TEST/nonexistent/config.yaml"
result=$(get_config "expertise" "")
assert_eq "get_config reads from settings.yaml config section" "frontend" "$result"
result=$(get_config "max_iterations" "40")
assert_eq "get_config reads numeric value from config section" "20" "$result"

# ---- AC2: Default value when key missing ----

echo ""
echo "AC2: Default value when key missing"
result=$(get_config "nonexistent_key" "fallback")
assert_eq "get_config returns default for missing key" "fallback" "$result"

# ---- AC3: Works when no settings files exist ----

echo ""
echo "AC3: Works when no settings files exist"
LOCAL_SETTINGS="$TMPDIR_TEST/nonexistent/settings.yaml"
GLOBAL_SETTINGS="$TMPDIR_TEST/nonexistent2/settings.yaml"
LEGACY_CONFIG="$TMPDIR_TEST/nonexistent3/config.yaml"
result=$(get_config "expertise" "")
assert_eq "get_config returns empty default when no files exist" "" "$result"

# ---- AC4: Local settings override global settings ----

echo ""
echo "AC4: Local settings override global settings"
mkdir -p "$TMPDIR_TEST/ac4_global" "$TMPDIR_TEST/ac4_local/.fno"
cat > "$TMPDIR_TEST/ac4_global/settings.yaml" <<'YAML'
config:
  expertise: backend
  budget_cap: 50
YAML
cat > "$TMPDIR_TEST/ac4_local/.fno/settings.yaml" <<'YAML'
config:
  expertise: frontend
YAML
GLOBAL_SETTINGS="$TMPDIR_TEST/ac4_global/settings.yaml"
LOCAL_SETTINGS="$TMPDIR_TEST/ac4_local/.fno/settings.yaml"
LEGACY_CONFIG="$TMPDIR_TEST/nonexistent/config.yaml"
result=$(get_config "expertise" "")
assert_eq "local settings override global" "frontend" "$result"
result=$(get_config "budget_cap" "25")
assert_eq "global settings used when key not in local" "50" "$result"

# ---- AC5: Legacy config.yaml fallback ----

echo ""
echo "AC5: Legacy config.yaml fallback"
mkdir -p "$TMPDIR_TEST/ac5/.fno"
cat > "$TMPDIR_TEST/ac5/.fno/config.yaml" <<'YAML'
expertise: legacy_value
YAML
LOCAL_SETTINGS="$TMPDIR_TEST/nonexistent/settings.yaml"
GLOBAL_SETTINGS="$TMPDIR_TEST/nonexistent2/settings.yaml"
LEGACY_CONFIG="$TMPDIR_TEST/ac5/.fno/config.yaml"
result=$(get_config "expertise" "")
assert_eq "legacy config.yaml used as fallback" "legacy_value" "$result"

# ---- config_is_true: truthy values ----

echo ""
echo "config_is_true: truthy/falsy values"
mkdir -p "$TMPDIR_TEST/ac_bool/.fno"
cat > "$TMPDIR_TEST/ac_bool/.fno/settings.yaml" <<'YAML'
config:
  no_external: true
  no_docs: false
  budget_cap: 20
YAML
LOCAL_SETTINGS="$TMPDIR_TEST/ac_bool/.fno/settings.yaml"
GLOBAL_SETTINGS="$TMPDIR_TEST/nonexistent/settings.yaml"
LEGACY_CONFIG="$TMPDIR_TEST/nonexistent/config.yaml"
if config_is_true "no_external"; then
    pass "config_is_true returns true for 'true' value"
else
    fail "config_is_true returns true for 'true' value"
fi
if ! config_is_true "no_docs"; then
    pass "config_is_true returns false for 'false' value"
else
    fail "config_is_true returns false for 'false' value"
fi
if ! config_is_true "missing_key"; then
    pass "config_is_true returns false for missing key"
else
    fail "config_is_true returns false for missing key"
fi

# ---- AC6: Nested YAML keys via yq ----

echo ""
echo "AC6: Nested YAML keys via yq"
mkdir -p "$TMPDIR_TEST/ac6/.fno"
cat > "$TMPDIR_TEST/ac6/.fno/settings.yaml" <<'YAML'
config:
  notifications:
    enabled: true
    channel: slack
YAML
LOCAL_SETTINGS="$TMPDIR_TEST/ac6/.fno/settings.yaml"
result=$(get_config "notifications.enabled" "false")
assert_eq "get_config handles nested key with dot notation" "true" "$result"
result=$(get_config "notifications.channel" "email")
assert_eq "get_config handles nested string key" "slack" "$result"

# ---- Summary ----

echo ""
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"

if [[ $FAIL -gt 0 ]]; then
    exit 1
fi
exit 0
