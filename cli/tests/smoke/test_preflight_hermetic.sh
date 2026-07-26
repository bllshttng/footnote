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

# The fallback must key on the fetch's EXIT STATUS, not only on empty output: a
# broken venv that prints a partial line before erroring must still fall back
# (AC4-ERR). A revert to `|| true` + a bare `-z` check reds here.
grep -Fq '&& [[ -n "$HARNESS_MARKERS" ]]; then' "$PF" \
  || fail "fallback not keyed on fetch exit status (partial-stdout+nonzero would slip past)"

# The changed packet must run through run_hermetic like every other leg, and
# with an EXPLICIT base/head. Local mode inside the preflight worktree would
# read its preserved untracked caches (target/, cli/.venv) as changed paths.
grep -Fq 'run_hermetic uv run --project cli fno-py test smoke --changed \' "$PF" \
  || fail "changed packet does not run inside run_hermetic"
grep -Fqe '--base "$CHANGED_BASE" --head "$CANDIDATE_SHA"' "$PF" \
  || fail "changed packet does not pin an explicit base/head"

# Only the full legs may mint FULL evidence. record_attestation must stay gated
# on the full path; the changed leg may only ever invalidate.
grep -Fq 'if [[ $RETRY_FAILED -eq 0 && $FAIL -eq 0 ]]; then' "$PF" \
  || fail "attestation is no longer gated on a full, non-subset, all-green run"
awk '/^CHANGED_BASE=""/,/^echo ""$/' "$PF" | grep -Fq 'record_attestation' \
  && fail "the changed leg can mint a FULL attestation" || true

echo "PASS: hermetic env scrubs harness markers + drops canonical config;"
echo "      changed packet is hermetic, base-pinned, and cannot mint FULL evidence"
