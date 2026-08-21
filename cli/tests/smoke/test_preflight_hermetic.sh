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

# Both marker lists preflight can end up using must cover AMBIENT_IDENTITY_ENV:
# the derivation it runs when Python works, and the literal it falls back to when
# Python does not. The fallback especially needs its own check - it only runs
# when the venv is broken, which is exactly when nobody is reading the output.
#
# Done in ONE python3 call that EXECUTES preflight's own derivation rather than
# grepping for tuple names. Two reasons. A name-occurrence grep passes on the
# unused import alone, so dropping the tuple from the executable expression while
# leaving the import reds nothing. And a shell `while read` over a here-string is
# silently skipped when bash cannot create the temp file backing `<<<`, under
# `set -e`, reaching PASS with zero assertions run (observed, not theorized).
# One command, one exit code, nothing to skip.
PROBLEMS="$(PYTHONPATH=cli/src python3 - "$PF" <<'PYEOF'
import re, subprocess, sys, os
from fno.harness_identity import AMBIENT_IDENTITY_ENV

source = open(sys.argv[1]).read()
expected = set(AMBIENT_IDENTITY_ENV)
problems = []

# The derivation preflight actually runs, executed rather than pattern-matched.
snippet = re.search(r"'(from fno\.harness_identity import [^']*)'", source)
if not snippet:
    problems.append("cannot locate the python derivation in preflight.sh")
else:
    env = dict(os.environ, PYTHONPATH="cli/src")
    out = subprocess.run([sys.executable, "-c", snippet.group(1)],
                         capture_output=True, text=True, env=env)
    if out.returncode != 0:
        problems.append(f"preflight's derivation does not run: {out.stderr.strip()}")
    else:
        derived = set(out.stdout.split())
        if missing := expected - derived:
            problems.append(f"derivation misses {sorted(missing)}")

# The last-resort literal.
literal = re.search(r'^\s*HARNESS_MARKERS="(CODEX_THREAD_ID[^"]*)"\s*$', source, re.M)
if not literal:
    problems.append("cannot locate the hardcoded fallback marker list")
elif missing := expected - set(literal.group(1).split()):
    problems.append(f"fallback literal misses {sorted(missing)}")

print("; ".join(problems))
PYEOF
)" || fail "harness marker coverage check did not run"
[[ -z "$PROBLEMS" ]] || fail "$PROBLEMS"

# The changed packet must run through run_hermetic like every other leg, and
# with an EXPLICIT base/head. Local mode inside the preflight worktree would
# read its preserved untracked caches (target/, cli/.venv) as changed paths.
grep -Fq 'run_hermetic uv run --project cli fno-py doctor test smoke --changed \' "$PF" \
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
