#!/usr/bin/env bash
# Bind-line contract tests for validate-plan.sh (x-f8b1 change 3).
#
# A passing run on a plan whose frontmatter names a node prints the exact
# bind command to stderr, so a session that stops one call short leaves the
# fix on its screen. Positive markers only: every assertion names a line the
# outcome PRODUCES, never a bare absence.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
VALIDATOR="$REPO_ROOT/skills/blueprint/scripts/validate-plan.sh"

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  ok: $*"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL: $*"; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

cat > "$TMP/node-bearing.md" <<'EOF'
---
node: x-7760
status: ready
created: 2026-08-01
project: test
---
# Test plan
EOF

cat > "$TMP/claims-only.md" <<'EOF'
---
claims:
  - x-7760
status: ready
created: 2026-08-01
project: test
---
# Test plan
EOF

cat > "$TMP/id-less.md" <<'EOF'
---
status: ready
created: 2026-08-01
project: test
---
# Test plan
EOF

run_case() {
    local name="$1" file="$2" expect_id="$3"
    local out err
    err="$TMP/$name.err"
    bash "$VALIDATOR" "$file" >"$TMP/$name.out" 2>"$err"
    local rc=$?
    if [ "$rc" -ne 0 ]; then
        fail "$name: validator exited $rc (expected 0)"
        return
    fi
    pass "$name: validator passes"
    if grep -q "fno backlog update $expect_id --plan-path $file" "$err"; then
        pass "$name: stderr names the bind command for $expect_id"
    else
        fail "$name: no bind command for $expect_id on stderr"
    fi
}

echo "== a passing run names the bind it cannot perform"
run_case node-bearing "$TMP/node-bearing.md" x-7760
run_case claims-only "$TMP/claims-only.md" x-7760

echo "== an id-less plan prints no bind line"
bash "$VALIDATOR" "$TMP/id-less.md" >"$TMP/id-less.out" 2>"$TMP/id-less.err"
if grep -q "fno backlog update" "$TMP/id-less.err"; then
    fail "id-less: bind line printed without a node in frontmatter"
else
    pass "id-less: no bind line"
fi

echo
echo "bind-line: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
