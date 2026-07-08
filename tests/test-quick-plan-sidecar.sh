#!/usr/bin/env bash
# test-quick-plan-sidecar.sh - verify sibling quick plans don't clobber each other's artifacts,
# and that session-state files are NOT dumped to sidecars (new contract: .completed/ gone).
#
# Runs the archive code path twice in a temp repo on plans/a.md then plans/b.md
# and asserts that:
#   - no .completed/ folder is created in either sidecar
#   - scratchpad-archive/ IS created in the sidecar when a scratchpad is present
#   - neither plan's artifacts clobber the shared plans/ root

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TMP=$(mktemp -d -t quick-plan-sidecar.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

cd "$TMP"
git init -q
mkdir -p plans .fno

cat > plans/a.md <<EOF
# Plan A

Quick plan A.
EOF
cat > plans/b.md <<EOF
# Plan B

Quick plan B.
EOF

# Write state file for a plan and seed a scratchpad so we can assert scratchpad-archive.
write_state_with_scratchpad() {
    local plan_name="$1"
    cat > .fno/target-state.md <<EOF
---
status: COMPLETE
input_type: plan
plan_path: plans/${plan_name}.md
input: plans/${plan_name}.md
created_at: 2026-04-20T00:00:00Z
iteration: 1
---

target-state-for-${plan_name}
EOF
    # Create a scratchpad with plan-specific content for scratchpad-archive assertions.
    mkdir -p .fno/scratchpad
    printf 'scratchpad content for plan %s\n' "$plan_name" > .fno/scratchpad/notes.md
}

# Plan A
write_state_with_scratchpad a
bash "$REPO_ROOT/tests/helpers/drive-target-archive.sh" .fno/target-state.md

# New contract: .completed/ must NOT exist in the sidecar.
[[ ! -d plans/a.md.artifacts/.completed ]] && pass "sidecar-a: no .completed/ dump" \
    || fail "sidecar-a: .completed/ still being created (expected absent)"
# Scratchpad-archive SHOULD exist (we seeded a scratchpad).
[[ -d plans/a.md.artifacts/scratchpad-archive ]] && pass "sidecar-a: scratchpad-archive present" \
    || fail "sidecar-a: scratchpad-archive missing (expected present)"
# The collision check: no artifacts land at the shared plans/ root.
[[ ! -f plans/HANDOFF.md && ! -e plans/.completed ]] && pass "plan A: no collision in plans/ root" \
    || fail "plan A: unexpected artifacts at plans/ root"

# Plan B
write_state_with_scratchpad b
bash "$REPO_ROOT/tests/helpers/drive-target-archive.sh" .fno/target-state.md

[[ ! -d plans/b.md.artifacts/.completed ]] && pass "sidecar-b: no .completed/ dump" \
    || fail "sidecar-b: .completed/ still being created (expected absent)"
[[ -d plans/b.md.artifacts/scratchpad-archive ]] && pass "sidecar-b: scratchpad-archive present" \
    || fail "sidecar-b: scratchpad-archive missing (expected present)"
# Sidecar A should still be intact.
[[ -d plans/a.md.artifacts/scratchpad-archive ]] && pass "sidecar-a scratchpad-archive survived plan B's run" \
    || fail "sidecar-a scratchpad-archive was clobbered by plan B"

echo
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"
exit "$FAIL"
