#!/usr/bin/env bash
# tests/ci/test_check_harness_capabilities_fresh.sh
#
# Test harness for scripts/ci/check-harness-capabilities-fresh.sh.
#
# Scenarios:
#   T01 - identical canonical and copy files -> PASS, rc=0
#   T02 - divergent files                   -> FAIL, names both paths and cp command, rc=1
#   T03 - missing copy                      -> misuse, rc=2
#   T04 - missing canonical                 -> misuse, rc=2
#
# Exit codes: 0 pass, 1 fail

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT_REAL="$(cd "${SCRIPT_DIR}/../.." && pwd)"
GATE="${REPO_ROOT_REAL}/scripts/ci/check-harness-capabilities-fresh.sh"

fail() { printf '[harness-caps-fresh-test] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[harness-caps-fresh-test] PASS: %s\n' "$*"; }

[[ -f "${GATE}" ]] || fail "gate script not found at ${GATE}"
bash -n "${GATE}" || fail "gate script failed bash -n"

TMP=$(mktemp -d -t harness-caps-test-XXXXXX)
trap 'rm -rf "$TMP"' EXIT

REPO="$TMP/repo"
CANONICAL_DIR="$REPO/crates/fno-agents/src"
COPY_DIR="$REPO/cli/src/fno/agents"

mkdir -p "$CANONICAL_DIR" "$COPY_DIR"
(cd "$REPO" && git init -q)

# T01: identical files exit 0
printf 'map_version = 10\n' > "$CANONICAL_DIR/harness_capabilities.toml"
cp "$CANONICAL_DIR/harness_capabilities.toml" "$COPY_DIR/harness_capabilities.toml"

out="$(cd "$REPO" && bash "${GATE}" 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 0 ]] || fail "T01: expected rc=0, got $rc: $out"
echo "$out" | grep -q "harness capabilities table fresh" || fail "T01: missing fresh message: $out"
pass "T01 matching files exit 0"

# T02: divergent files exit 1 and output paths + cp command on stderr
printf '\n# extra line\n' >> "$COPY_DIR/harness_capabilities.toml"
out="$(cd "$REPO" && bash "${GATE}" 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 1 ]] || fail "T02: expected rc=1, got $rc: $out"
echo "$out" | grep -q "crates/fno-agents/src/harness_capabilities.toml" || fail "T02: canonical path not named: $out"
echo "$out" | grep -q "cli/src/fno/agents/harness_capabilities.toml" || fail "T02: copy path not named: $out"
echo "$out" | grep -q "cp crates/fno-agents/src/harness_capabilities.toml cli/src/fno/agents/harness_capabilities.toml" || fail "T02: cp command not suggested: $out"
pass "T02 divergent files fail (rc=1), naming paths and cp command"

# Restore matching state
cp "$CANONICAL_DIR/harness_capabilities.toml" "$COPY_DIR/harness_capabilities.toml"

# T03: missing copy exits 2
rm -f "$COPY_DIR/harness_capabilities.toml"
out="$(cd "$REPO" && bash "${GATE}" 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 2 ]] || fail "T03: expected rc=2, got $rc: $out"
echo "$out" | grep -q "missing" || fail "T03: missing error message expected: $out"
pass "T03 missing copy exits 2"

# T04: missing canonical exits 2
cp "$CANONICAL_DIR/harness_capabilities.toml" "$COPY_DIR/harness_capabilities.toml"
rm -f "$CANONICAL_DIR/harness_capabilities.toml"
out="$(cd "$REPO" && bash "${GATE}" 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 2 ]] || fail "T04: expected rc=2, got $rc: $out"
echo "$out" | grep -q "missing" || fail "T04: missing error message expected: $out"
pass "T04 missing canonical exits 2"

pass "all scenarios passed"
