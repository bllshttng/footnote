#!/usr/bin/env bash
# Test scripts/lib/state-parser.sh - shared field extractor for state files.
# Added per Gemini PR #188 review (medium-priority centralization request).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
LIB="${REPO_ROOT}/scripts/lib/state-parser.sh"

log()  { printf '[state-parser] %s\n' "$*"; }
fail() { printf '[state-parser] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[state-parser] PASS: %s\n' "$*"; }
[[ -f "$LIB" ]] || fail "lib not found at $LIB"

bash -n "$LIB" || fail "bash -n rejected $LIB"
pass "lib passes bash -n"

source "$LIB"
declare -F read_state_field >/dev/null || fail "read_state_field not defined after source"
pass "read_state_field defined"

TMP=$(mktemp -d -t state-parser-XXXXXX)
trap 'rm -rf "$TMP"' EXIT

# Scenario 1: bare scalar
STATE="$TMP/bare.md"
cat > "$STATE" <<'EOF'
---
status: COMPLETE
session_id: abc-123
pr_number: 188
---
EOF
[[ "$(read_state_field "$STATE" status)" == "COMPLETE" ]] || fail "bare scalar"
[[ "$(read_state_field "$STATE" session_id)" == "abc-123" ]] || fail "bare session_id"
[[ "$(read_state_field "$STATE" pr_number)" == "188" ]] || fail "bare pr_number"
pass "scenario 1: bare scalar values"

# Scenario 2: double-quoted strings
STATE="$TMP/quoted.md"
cat > "$STATE" <<'EOF'
---
session_id: "abc-123"
pr_url: "https://github.com/x/y/pull/188"
---
EOF
[[ "$(read_state_field "$STATE" session_id)" == "abc-123" ]] || fail "double-quoted session_id"
[[ "$(read_state_field "$STATE" pr_url)" == "https://github.com/x/y/pull/188" ]] || fail "double-quoted pr_url"
pass "scenario 2: double-quoted scalars stripped"

# Scenario 3: single-quoted strings
STATE="$TMP/squoted.md"
cat > "$STATE" <<'EOF'
---
graph_node_id: 'ab-12345678'
EOF
[[ "$(read_state_field "$STATE" graph_node_id)" == "ab-12345678" ]] || fail "single-quoted graph_node_id"
pass "scenario 3: single-quoted scalars stripped"

# Scenario 4: literal "null" -> empty string
STATE="$TMP/null.md"
cat > "$STATE" <<'EOF'
---
plan_path: null
EOF
[[ -z "$(read_state_field "$STATE" plan_path)" ]] || fail "null should yield empty"
pass "scenario 4: literal null normalized to empty"

# Scenario 5: missing field
STATE="$TMP/missing.md"
cat > "$STATE" <<'EOF'
---
status: COMPLETE
EOF
[[ -z "$(read_state_field "$STATE" no_such_field)" ]] || fail "missing field should yield empty"
pass "scenario 5: missing field yields empty"

# Scenario 6: missing file (no error, empty result)
out=$(read_state_field "$TMP/does-not-exist.md" status)
[[ -z "$out" ]] || fail "missing file should yield empty"
pass "scenario 6: missing file yields empty"

# Scenario 7: file with trailing whitespace
STATE="$TMP/trailing.md"
printf -- "---\nsession_id: abc   \n---\n" > "$STATE"
[[ "$(read_state_field "$STATE" session_id)" == "abc" ]] || fail "trailing whitespace not stripped"
pass "scenario 7: trailing whitespace stripped"

log ""
log "all state-parser scenarios passed"
