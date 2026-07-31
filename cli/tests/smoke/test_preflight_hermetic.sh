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

# The derivation must read the LEGACY tuple too. CLAUDE_SESSION_ID lives only
# there, and current_session_id()/current_session_ids() both read it, so a
# canonical-tuple-only derivation leaves a live claude session resolvable.
grep -Fq 'LEGACY_HARNESS_SESSION_MARKERS' "$PF" \
  || fail "marker derivation ignores LEGACY_HARNESS_SESSION_MARKERS"

# The last-resort literal must still name every marker. A stale copy scrubs less
# than the derived list, and the gap is invisible where it matters: the fallback
# only runs when the venv is broken, which is exactly when nobody is reading.
EXPECTED_MARKERS="$(PYTHONPATH=cli/src python3 -c \
  'from fno.harness_identity import HARNESS_SESSION_MARKERS as C, LEGACY_HARNESS_SESSION_MARKERS as L; print("\n".join(m[0] for m in (*C, *L)))')" \
  || fail "cannot read the harness marker tuples"
[[ -n "$EXPECTED_MARKERS" ]] || fail "harness marker tuples resolved empty"
FALLBACK_MARKERS="$(sed -n 's/^ *HARNESS_MARKERS="\(CODEX_THREAD_ID[^"]*\)"$/\1/p' "$PF" | tr ' ' '\n')"
[[ -n "$FALLBACK_MARKERS" ]] || fail "cannot locate the hardcoded fallback marker list"
while read -r marker; do
  printf '%s\n' "$FALLBACK_MARKERS" | grep -qx -- "$marker" \
    || fail "hardcoded fallback marker list is missing $marker"
done <<< "$EXPECTED_MARKERS"

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

grep -Fq 'receipt start timestamp unavailable' "$PF" \
  || fail "receipt start timestamp discovery does not fail closed"
grep -Fq 'receipt host identity unavailable' "$PF" \
  || fail "receipt host discovery does not fail closed"
grep -Fq 'receipt platform identity unavailable' "$PF" \
  || fail "receipt platform discovery does not fail closed"
grep -Fq -- '--arg host "$RECEIPT_HOST"' "$PF" \
  || fail "receipt does not bind the discovered host"
grep -Fq -- '--arg platform "$RECEIPT_PLATFORM"' "$PF" \
  || fail "receipt does not bind the discovered platform"
grep -Fq 'except FileNotFoundError:' "$PF" \
  || fail "squads probe does not distinguish absent"
grep -Fq "print('unavailable')" "$PF" \
  || fail "squads probe does not preserve unavailable"
grep -Fq 'RECEIPT_RESULT=unavailable' "$PF" \
  || fail "unavailable required evidence does not reach the final receipt"
grep -Fq 'from fno.events import append_event' "$PF" \
  || fail "verification receipts do not use the shared journal append primitive"
grep -Fq 'append_event(event, events_path=Path(sys.argv[1]))' "$PF" \
  || fail "verification receipts do not bind the shared append primitive to each journal"
if grep -Fq '>> "$GLOBAL_EVENTS_PATH"' "$PF" \
    || grep -Fq '>> "$EVENTS_PATH"' "$PF"; then
  fail "verification receipts still bypass the per-journal mutex"
fi

echo "PASS: hermetic env scrubs harness markers + drops canonical config;"
echo "      changed packet and verification evidence contracts are preserved"
