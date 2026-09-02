#!/usr/bin/env bash
# tests/ci/test_check_harness_capabilities_fresh.sh
#
# Test harness for scripts/ci/check-harness-capabilities-fresh.sh.
#
# One canonical table (crates/fno-agents, which build.rs reads) and TWO
# generated copies (the Python tree's, which the package loads as a resource,
# and the mux crate's, which include_str!s it), so the scenarios cover the
# canonical-plus-copies set.
#
# Scenarios:
#   T01 - identical canonical and copies  -> PASS, rc=0
#   T02 - one divergent copy              -> FAIL, names both paths, rc=1
#   T03 - missing copy                    -> misuse, rc=2
#   T04 - missing canonical               -> misuse, rc=2
#   T05 - committed drift, worktree resynced by a build -> FAIL, rc=1
#   T06 - --write resyncs divergent copies -> PASS, rc=0
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
CLI_COPY_DIR="$REPO/cli/src/fno/agents"
MUX_COPY_DIR="$REPO/crates/fno/src"

mkdir -p "$CANONICAL_DIR" "$CLI_COPY_DIR" "$MUX_COPY_DIR"
(cd "$REPO" && git init -q)

# T01: identical files exit 0
printf 'map_version = 10\n' > "$CANONICAL_DIR/harness_capabilities.toml"
cp "$CANONICAL_DIR/harness_capabilities.toml" "$CLI_COPY_DIR/harness_capabilities.toml"
cp "$CANONICAL_DIR/harness_capabilities.toml" "$MUX_COPY_DIR/harness_capabilities.toml"

out="$(cd "$REPO" && bash "${GATE}" 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 0 ]] || fail "T01: expected rc=0, got $rc: $out"
echo "$out" | grep -q "harness capabilities table fresh" || fail "T01: missing fresh message: $out"
pass "T01 matching files exit 0"

# T02: one divergent copy exits 1 and outputs paths + resync hint on stderr
printf '\n# extra line\n' >> "$MUX_COPY_DIR/harness_capabilities.toml"
out="$(cd "$REPO" && bash "${GATE}" 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 1 ]] || fail "T02: expected rc=1, got $rc: $out"
echo "$out" | grep -q "crates/fno/src/harness_capabilities.toml" || fail "T02: mux copy path not named: $out"
echo "$out" | grep -q "crates/fno-agents/src/harness_capabilities.toml" || fail "T02: canonical path not named: $out"
echo "$out" | grep -q "cargo build -p fno-agents" || fail "T02: resync hint not suggested: $out"
pass "T02 divergent copy fails (rc=1), naming paths and resync hint"

# Restore matching state
cp "$CANONICAL_DIR/harness_capabilities.toml" "$MUX_COPY_DIR/harness_capabilities.toml"

# T03: missing copy exits 2
rm -f "$CLI_COPY_DIR/harness_capabilities.toml"
out="$(cd "$REPO" && bash "${GATE}" 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 2 ]] || fail "T03: expected rc=2, got $rc: $out"
echo "$out" | grep -q "missing" || fail "T03: missing error message expected: $out"
pass "T03 missing copy exits 2"

# T04: missing canonical exits 2
cp "$CANONICAL_DIR/harness_capabilities.toml" "$CLI_COPY_DIR/harness_capabilities.toml"
rm -f "$CANONICAL_DIR/harness_capabilities.toml"
out="$(cd "$REPO" && bash "${GATE}" 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 2 ]] || fail "T04: expected rc=2, got $rc: $out"
echo "$out" | grep -q "missing" || fail "T04: missing error message expected: $out"
pass "T04 missing canonical exits 2"

# T05: committed drift survives a build that resyncs the worktree.
#
# This is the ordering hazard the producer creates. `cargo build` overwrites the
# worktree copies, so a worktree-only comparison would go green on a hand edit
# that is still committed. The default committed-bytes comparison must still
# fail. Needs real commits, so this scenario builds its own repo.
REPO2="$TMP/repo2"
CANONICAL_DIR2="$REPO2/crates/fno-agents/src"
COPY_DIR2="$REPO2/crates/fno/src"
CLI_COPY_DIR2="$REPO2/cli/src/fno/agents"
mkdir -p "$CANONICAL_DIR2" "$COPY_DIR2" "$CLI_COPY_DIR2"
(
  cd "$REPO2"
  git init -q
  git config user.email "test@example.invalid"
  git config user.name "gate selftest"
)
printf 'map_version = 10\n' > "$CANONICAL_DIR2/harness_capabilities.toml"
cp "$CANONICAL_DIR2/harness_capabilities.toml" "$CLI_COPY_DIR2/harness_capabilities.toml"
# The committed copy carries a hand edit the canonical never had.
printf 'map_version = 10\n# hand edit\n' > "$COPY_DIR2/harness_capabilities.toml"
(cd "$REPO2" && git add -A && git commit -q -m "committed drift")

# Simulate the build: the producer overwrites the WORKTREE copy.
cp "$CANONICAL_DIR2/harness_capabilities.toml" "$COPY_DIR2/harness_capabilities.toml"

out="$(cd "$REPO2" && bash "${GATE}" 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 1 ]] || fail "T05: expected rc=1 on committed drift, got $rc: $out"
echo "$out" | grep -q "has diverged" || fail "T05: divergence not reported: $out"
echo "$out" | grep -q "committed bytes" || fail "T05: did not compare committed bytes: $out"
pass "T05 committed drift fails even after a build resyncs the worktree"

# And --worktree agrees with the resynced tree, so the two modes are distinct.
out="$(cd "$REPO2" && bash "${GATE}" --worktree 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 0 ]] || fail "T05b: expected --worktree rc=0, got $rc: $out"
pass "T05b --worktree passes on the resynced worktree"

# T06: --write resyncs divergent copies and exits 0.
printf '# hand edit\n' >> "$COPY_DIR2/harness_capabilities.toml"
out="$(cd "$REPO2" && bash "${GATE}" --write 2>&1)" && rc=0 || rc=$?
[[ "$rc" -eq 0 ]] || fail "T06: expected rc=0 from --write, got $rc: $out"
cmp -s "$CANONICAL_DIR2/harness_capabilities.toml" "$COPY_DIR2/harness_capabilities.toml" \
  || fail "T06: --write left the generated copy divergent"
pass "T06 --write resyncs the copy"

pass "all scenarios passed"
