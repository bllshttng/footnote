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

# --- 3. The installed interpreter is found even with fno-py off PATH ---------
# On a cargo-only install only the Rust mux (~/.cargo/bin/fno) is on PATH; fno-py
# is not, and that machine has no checkout venv to fall back on. Hermetic: a fake
# shim under a fake HOME, so this asserts the resolver's own legs, not the box.
FAKEHOME="$(mktemp -d)"
mkdir -p "$FAKEHOME/.local/bin"
printf '#!%s\n' "$(command -v sh)" > "$FAKEHOME/.local/bin/fno-py"
NOROOT="$(mktemp -d)"   # no cli/.venv and no cli/src, so leg 1 cannot match
# PATH keeps the standard bin dirs (the resolver itself needs sed/awk) but drops
# ~/.local/bin, which is exactly the cargo-only shape.
( HOME="$FAKEHOME" PATH="/usr/bin:/bin" fno_python_init "$NOROOT"
  [[ "$FNO_PYTHON" == "$(command -v sh)" ]] ) \
    && pass "reads the installed fno-py shebang with fno-py off PATH" \
    || fail "missed the installed interpreter off PATH - would fall to bare python3"
rm -rf "$FAKEHOME" "$NOROOT"

# --- 4. Always resolves something (degrade, never break the caller) -----------
( fno_python_init "$TMP"; [[ -n "$FNO_PYTHON" ]] ) \
    && pass "always sets FNO_PYTHON" \
    || fail "left FNO_PYTHON empty - caller would exec the empty string"

# --- 5. No production script reintroduces the bare-python3 ledger write -------
# The regression guard for the class: the callers must route through the
# resolver. Tests may still shell a bare python3; only shipped scripts may not.
# Scoped to *.sh, since prose that merely NAMES the bad form is not a caller -
# the LOC-ratchet trajectory quotes it verbatim, and every shipped shell script
# in these trees carries the extension. Comment lines are excluded too: the
# resolver's own header names the form it exists to prevent (the match is on
# `file:line:` followed by a `#`).
STRAY="$(grep -rn --include='*.sh' 'python3 -m fno\.cost\._register' \
    scripts/ hooks/ skills/ 2>/dev/null | grep -v ':[[:space:]]*#' || true)"
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
