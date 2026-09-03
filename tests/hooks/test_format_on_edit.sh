#!/usr/bin/env bash
# Test suite for hooks/format-on-edit.sh.
#
# The hook formats a .rs or .py file right after an Edit or a Write, so an
# agent never spends a gated Bash call on `cargo fmt`. Its one invariant: a
# formatter must NEVER fail the edit, so every failure path exits 0 silently.
#
# Tests:
#   T1  an unformatted .rs under crates/  -> rewritten, "format-on-edit: rewrote"
#   T2  an already-formatted .rs          -> exit 0, silent (no needless line)
#   T3  a .md path                        -> exit 0, silent
#   T4  a .rs OUTSIDE crates/             -> exit 0, silent, file untouched
#   T5  a .rs with a syntax error         -> exit 0, silent (never fails the edit)
#   T6  a missing file / empty payload    -> exit 0, silent

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/format-on-edit.sh"
PINNED_FMT="1.94.1"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); printf '[format-on-edit] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[format-on-edit] FAIL: %s\n' "$*" >&2; }

[[ -f "$HOOK" ]] || { fail "hook not found at $HOOK"; exit 1; }
if ! rustfmt "+${PINNED_FMT}" --version >/dev/null 2>&1; then
  echo "[format-on-edit] SKIP: rustfmt +${PINNED_FMT} not installed"
  exit 77
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
REPO="${TMP_DIR}/repo"
mkdir -p "${REPO}/crates/demo/src" "${REPO}/docs" "${REPO}/scripts"
# A bare .git DIRECTORY is enough: the hook walks up looking for `.git`, it
# never runs a git command.
mkdir -p "${REPO}/.git"

run_hook() {
  python3 -c '
import json, sys
print(json.dumps({"tool_name": "Edit", "tool_input": {"file_path": sys.argv[1]}}))
' "$1" | bash "$HOOK"
}

UGLY='fn  main( ) {let x=1;println!("{}",x);}'

# T1: an unformatted .rs under crates/ is rewritten and the line is printed.
F="${REPO}/crates/demo/src/lib.rs"
printf '%s\n' "$UGLY" > "$F"
OUT="$(run_hook "$F")"; RC=$?
if [[ $RC -eq 0 && "$OUT" == *"format-on-edit: rewrote crates/demo/src/lib.rs"* ]] \
   && ! grep -q 'fn  main' "$F"; then
  pass "T1 an unformatted .rs under crates/ is rewritten and announced"
else
  fail "T1 rc=$RC out='$OUT' file=$(cat "$F")"
fi

# T2: running again changes nothing, so the hook stays silent.
OUT="$(run_hook "$F")"
if [[ -z "$OUT" ]]; then
  pass "T2 an already-formatted file produces no output"
else
  fail "T2 expected silence, got '$OUT'"
fi

# T3: a .md path is not this hook's business.
M="${REPO}/docs/notes.md"
printf 'a  b\n' > "$M"
OUT="$(run_hook "$M")"; RC=$?
if [[ $RC -eq 0 && -z "$OUT" && "$(cat "$M")" == 'a  b' ]]; then
  pass "T3 a .md path exits 0 silently and is untouched"
else
  fail "T3 rc=$RC out='$OUT'"
fi

# T4: a .rs outside crates/ is left alone - the pin is scoped to the workspace.
S="${REPO}/scripts/stray.rs"
printf '%s\n' "$UGLY" > "$S"
OUT="$(run_hook "$S")"; RC=$?
if [[ $RC -eq 0 && -z "$OUT" ]] && grep -q 'fn  main' "$S"; then
  pass "T4 a .rs outside crates/ is untouched"
else
  fail "T4 rc=$RC out='$OUT'"
fi

# T5: rustfmt refuses a file it cannot parse. The edit must still succeed.
B="${REPO}/crates/demo/src/broken.rs"
printf 'fn main( { unbalanced\n' > "$B"
OUT="$(run_hook "$B")"; RC=$?
if [[ $RC -eq 0 && -z "$OUT" ]]; then
  pass "T5 a file rustfmt rejects exits 0 silently"
else
  fail "T5 rc=$RC out='$OUT'"
fi

# T6: a payload naming a file that does not exist.
OUT="$(run_hook "${REPO}/crates/demo/src/gone.rs")"; RC=$?
if [[ $RC -eq 0 && -z "$OUT" ]]; then
  pass "T6 a missing file exits 0 silently"
else
  fail "T6 rc=$RC out='$OUT'"
fi

# T7: Python is deliberately NOT formatted, and that is pinned by a test rather
# than only by a comment. CI runs `ruff check`, never `ruff format`, so the repo
# has never been ruff-formatted; formatting on edit would put that churn in
# every PR touching a Python file and satisfy no gate.
mkdir -p "${REPO}/cli/src"
P="${REPO}/cli/src/thing.py"
printf 'x  =  1\n' > "$P"
OUT="$(run_hook "$P")"; RC=$?
if [[ $RC -eq 0 && -z "$OUT" && "$(cat "$P")" == 'x  =  1' ]]; then
  pass "T7 a .py file is left alone (no ruff format leg)"
else
  fail "T7 rc=$RC out='$OUT' file=$(cat "$P")"
fi

printf '[format-on-edit] %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
