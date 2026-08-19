#!/usr/bin/env bash
# tests/ci/test-check-disposable-rm.sh
#
# Exercises scripts/ci/check-disposable-rm.sh. The gate must FAIL on a bare
# `rm` in a guarded file (AC6) and PASS everywhere else (AC7: it is an
# allowlist, never a repo-wide ban). Every case asserts an exit code; a PASS
# here is only meaningful because the FAIL cases exist too.
#
# Run: bash tests/ci/test-check-disposable-rm.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$(cd "${SCRIPT_DIR}/../.." && pwd)/scripts/ci/check-disposable-rm.sh"
TMP="$(mktemp -d)"
trap 'command -p rm -rf "$TMP" 2>/dev/null || /bin/rm -rf "$TMP"' EXIT

PASS=0; FAIL=0
[[ -f "$GATE" ]] || { echo "gate not found at $GATE" >&2; exit 1; }

# run <expected_exit> <label> <args...>: assert the gate's exit code
run() {
  local want="$1" label="$2"; shift 2
  local got
  bash "$GATE" "$@" >/dev/null 2>&1; got=$?
  if [[ "$got" -eq "$want" ]]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s\n  want exit %s, got %s\n' "$label" "$want" "$got"
  fi
}

# --- AC6: a bare rm in a guarded file fails -----------------------------------
cat > "$TMP/locked.sh" <<'EOF'
set -uo pipefail
rm -rf "$LOCKDIR.reap.$$"
mkdir "$LOCKDIR" 2>/dev/null
EOF
run 1 'bare rm -rf fails' "$TMP/locked.sh"

cat > "$TMP/flagged.sh" <<'EOF'
rm -f "$tmp" 2>/dev/null || true
EOF
run 1 'bare rm -f fails' "$TMP/flagged.sh"

# `command rm` bypasses functions and aliases only, not a PATH entry.
cat > "$TMP/commandrm.sh" <<'EOF'
command rm -rf "$X"
EOF
run 1 'command rm (no -p) fails: still hits a PATH wrapper' "$TMP/commandrm.sh"

# A guarded file that vanished fails closed.
run 1 'missing listed file fails closed' "$TMP/does-not-exist.sh"

# --- sanctioned spellings pass -------------------------------------------------
cat > "$TMP/tworung.sh" <<'EOF'
command -p rm -rf "$LOCKDIR.reap.$$" 2>/dev/null || /bin/rm -rf "$LOCKDIR.reap.$$"
command -p rm -f "$s" 2>/dev/null || /bin/rm -f "$s" 2>/dev/null || true
{ command -p rm -f "$ATTEST" 2>/dev/null || /bin/rm -f "$ATTEST"; } && echo gone
EOF
run 0 'two-rung spellings pass' "$TMP/tworung.sh"

# --- no false positives on prose ------------------------------------------------
cat > "$TMP/prose.sh" <<'EOF'
# rm -rf, a loser deletes the lockdir the winner just recreated.
echo "preflight: if no preflight is running, remove it: rm -rf '$LOCKDIR'" >&2
git worktree prune >/dev/null 2>&1 || true  # drop entries from a prior rm -rf
EOF
run 0 'rm inside comments and echo strings passes' "$TMP/prose.sh"

# --- AC7: the default (allowlist) run is not a repo-wide ban -------------------
# The fixture above is full of bare rm; the default run ignores it because it
# is not on the allowlist. The real guarded files must pass as shipped.
run 0 'default allowlist run passes on the shipped tree'

# --- the failure message teaches: names file, line, criterion -------------------
# Capture first, grep after. A grep -q on a live pipe closes it early, and the
# gate then SIGPIPEs on its next write. pipefail reads that as a false FAIL
# (the trap preflight.sh's is_registered comment records).
MSG="$(bash "$GATE" "$TMP/locked.sh" 2>&1 || true)"
if printf '%s\n' "$MSG" | grep -q "$TMP/locked.sh:2" \
   && printf '%s\n' "$MSG" | grep -q 'command -p rm'; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1)); echo 'FAIL: failure message must name file:line and the fix'
fi

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
