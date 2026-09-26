#!/usr/bin/env bash
# Test suite for scripts/ci/check-edit-integrity.sh, the branch-diff door
# the smoke runner discovers under scripts/tests/.
#
# Tests:
#   T1  a diff that drops a test and a .md newline -> both findings, exit 0
#   T2  an unresolvable EDIT_INTEGRITY_BASE        -> exit 2
#   T3  no binary answers (stub exits 2)           -> the remedy, exit 3

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/ci/check-edit-integrity.sh"

[[ -f "$SCRIPT" ]] || { echo "FAIL: script not found at $SCRIPT" >&2; exit 1; }
BIN="${FNO_AGENTS_BIN:-}"
if [[ -z "$BIN" ]]; then
    for candidate in "$REPO_ROOT/crates/fno-agents/target/release/fno-agents" \
        "$REPO_ROOT/crates/fno-agents/target/debug/fno-agents"; do
        [[ -x "$candidate" ]] && BIN="$candidate" && break
    done
fi
if [[ -z "$BIN" ]] || [[ ! -x "$BIN" ]]; then
    (cd "$REPO_ROOT/crates/fno-agents" && cargo build --bin fno-agents >/dev/null 2>&1)
    BIN="$REPO_ROOT/crates/fno-agents/target/debug/fno-agents"
fi
[[ -x "$BIN" ]] || { echo "FAIL: fno-agents binary not executable at $BIN" >&2; exit 1; }
export FNO_AGENTS_BIN="$BIN"

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  PASS: $*"; }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t check-edit-integrity-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

REPO="$TMP/repo"
mkdir -p "$REPO/docs"
/usr/bin/git -C "$REPO" init -q
cat > "$REPO/test_x.py" <<'PYEOF'
def test_a():
    pass

def test_b():
    pass

def test_c():
    pass
PYEOF
printf 'hello\n' > "$REPO/docs/a.md"
/usr/bin/git -C "$REPO" add .
/usr/bin/git -C "$REPO" -c user.email=t@t -c user.name=t commit -qm base
BASE_SHA="$(/usr/bin/git -C "$REPO" rev-parse HEAD)"

# The second commit: one test gone, the .md's final newline gone.
cat > "$REPO/test_x.py" <<'PYEOF'
def test_a():
    pass

def test_b():
    pass
PYEOF
printf 'hello' > "$REPO/docs/a.md"
/usr/bin/git -C "$REPO" add .
/usr/bin/git -C "$REPO" -c user.email=t@t -c user.name=t commit -qm cut

# T1: both findings print over the two-commit diff; advisory only, so 0.
OUT="$(cd "$REPO" && EDIT_INTEGRITY_BASE="$BASE_SHA" bash "$SCRIPT")"
RC=$?
if [[ $RC -eq 0 && "$OUT" == *"test count fell 3 -> 2"* \
    && "$OUT" == *"docs/a.md"*"last line has no newline"* ]]; then
  pass "T1 both findings print, exit 0"
else
  fail "T1 rc=$RC out='$OUT'"
fi

# T2: an unresolvable base refuses with exit 2.
OUT="$(cd "$REPO" && EDIT_INTEGRITY_BASE=not-a-ref bash "$SCRIPT" 2>/dev/null)"
RC=$?
if [[ $RC -eq 2 ]]; then
  pass "T2 an unresolvable base exits 2"
else
  fail "T2 rc=$RC out='$OUT'"
fi

# T3: a stub that exits 2 on PATH and no other binary answers: the remedy
# prints and the exit is 3.
mkdir -p "$TMP/stubbin"
printf '#!/bin/sh\nexit 2\n' > "$TMP/stubbin/fno-agents"
chmod +x "$TMP/stubbin/fno-agents"
OUT="$(cd "$REPO" && env -u FNO_AGENTS_BIN EDIT_INTEGRITY_BASE="$BASE_SHA" PATH="$TMP/stubbin" /bin/bash "$SCRIPT" 2>&1)"
RC=$?
if [[ $RC -eq 3 && "$OUT" == *"cargo build --bin fno-agents"* ]]; then
  pass "T3 no answering binary names the remedy, exit 3"
else
  fail "T3 rc=$RC out='$OUT'"
fi

# T4: a deletion in the diff is judged as emptied content, not skipped.
/usr/bin/git -C "$REPO" rm -q docs/a.md
/usr/bin/git -C "$REPO" -c user.email=t@t -c user.name=t commit -qm drop-md
OUT="$(cd "$REPO" && EDIT_INTEGRITY_BASE="$BASE_SHA" bash "$SCRIPT" 2>/dev/null)"
RC=$?
if [[ $RC -eq 0 && "$OUT" == *"docs/a.md"*"file is now empty"* ]]; then
  pass "T4 a deleted file is judged as emptied content"
else
  fail "T4 rc=$RC out='$OUT'"
fi

echo "[check-edit-integrity] $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
