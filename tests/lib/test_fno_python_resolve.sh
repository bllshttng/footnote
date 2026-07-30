#!/usr/bin/env bash
# tests/lib/test_fno_python_resolve.sh
#
# Guards scripts/lib/fno-python.sh, the interpreter resolver the ledger writers
# use. The bug it exists for: a bare `python3 -m fno.cost._register` runs under
# whatever python is first on PATH, and one without fno's deps dies inside
# fno.paths on `import fno.config` - so the ledger row is dropped while the
# caller (which sends stderr to a log) reports nothing.

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

FAILURES=0
pass() { printf 'ok   - %s\n' "$1"; }
fail() { printf 'FAIL - %s\n' "$1"; FAILURES=$((FAILURES + 1)); }

# shellcheck source=../../scripts/lib/fno-python.sh
source "${REPO_ROOT}/scripts/lib/fno-python.sh"

# --- 1. The resolved interpreter can actually load the module that broke ------
# This is the load-bearing assertion: `import fno.config` is the exact import
# that dropped ledger rows. Only meaningful when a checkout venv exists (the
# canonical one, since a linked worktree carries cli/src but no cli/.venv).
CANONICAL="$(git worktree list --porcelain | awk 'NR==1{sub(/^worktree /,"");print}')"
if [[ -x "${CANONICAL}/cli/.venv/bin/python3" ]]; then
    ( fno_python_init "$REPO_ROOT"
      "$FNO_PYTHON" -c "import fno.config" 2>/dev/null ) \
        && pass "resolved interpreter imports fno.config" \
        || fail "resolved interpreter cannot import fno.config (ledger rows would be dropped)"

    ( fno_python_init "$REPO_ROOT"; [[ "$FNO_PYTHON" != "python3" ]] ) \
        && pass "resolves a real path, not bare python3, inside a checkout" \
        || fail "fell through to bare python3 despite a usable checkout venv"
else
    printf 'skip - no canonical cli/.venv; interpreter-capability checks need one\n'
fi

# --- 2. A foreign cli/.venv without fno's package source is refused -----------
# Selecting it would trade one silent drop for another: `import fno` fails there.
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/cli/.venv/bin"
printf '#!/bin/sh\nexit 0\n' > "$TMP/cli/.venv/bin/python3"
chmod +x "$TMP/cli/.venv/bin/python3"
( fno_python_init "$TMP"; [[ "$FNO_PYTHON" != "$TMP/cli/.venv/bin/python3" ]] ) \
    && pass "refuses a cli/.venv with no co-located cli/src/fno" \
    || fail "selected a foreign cli/.venv where 'import fno' would fail"

# --- 3. Always resolves something (degrade, never break the caller) -----------
( fno_python_init "$TMP"; [[ -n "$FNO_PYTHON" ]] ) \
    && pass "always sets FNO_PYTHON" \
    || fail "left FNO_PYTHON empty - caller would exec the empty string"

# --- 4. No production script reintroduces the bare-python3 ledger write -------
# The regression guard for the class: the callers must route through the
# resolver. Tests may still shell a bare python3; only shipped scripts may not.
# Comment lines are excluded - the resolver's own docstring names the bad form
# it exists to prevent (the match is on `file:line:` followed by a `#`).
STRAY="$(grep -rn 'python3 -m fno\.cost\._register' scripts/ hooks/ skills/ 2>/dev/null \
    | grep -v ':[[:space:]]*#' || true)"
if [[ -z "$STRAY" ]]; then
    pass "no production caller shells a bare 'python3 -m fno.cost._register'"
else
    fail "bare-python3 ledger write reintroduced:"
    printf '%s\n' "$STRAY"
fi

if (( FAILURES > 0 )); then
    printf '\n%d check(s) failed\n' "$FAILURES"
    exit 1
fi
printf '\nall checks passed\n'
