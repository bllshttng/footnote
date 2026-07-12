#!/usr/bin/env bash
# The hermetic preflight env must seal both ambient leaks (x-bbe7): scrub the
# HARNESS_SESSION_MARKERS names and export FNO_NO_CANONICAL_CONFIG=1. A static
# assertion over run_hermetic's body - a later refactor that drops either seam
# reds here instead of only for someone running preflight inside a configured
# worktree.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
PF="scripts/ci/preflight.sh"

fail() { echo "FAIL: $1"; exit 1; }

grep -Fq 'for v in $HARNESS_MARKERS; do unset "$v"; done' "$PF" \
  || fail "run_hermetic does not unset HARNESS_MARKERS"
grep -Fq 'export FNO_NO_CANONICAL_CONFIG=1' "$PF" \
  || fail "run_hermetic does not export FNO_NO_CANONICAL_CONFIG=1"

# The marker list is derived from the Python single source of truth, with a
# fail-closed literal fallback (never a silent skip).
grep -Fq 'HARNESS_SESSION_MARKERS' "$PF" \
  || fail "marker list not sourced from HARNESS_SESSION_MARKERS"
grep -Fq 'hardcoded fallback list' "$PF" \
  || fail "no fail-closed fallback for the marker fetch"

echo "PASS: hermetic env scrubs harness markers + drops canonical config"
