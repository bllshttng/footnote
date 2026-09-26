#!/usr/bin/env bash
# Test suite for hooks/edit-integrity.sh over a throwaway git repo.
#
# The shim is a probe-and-relay: it collects the paths the payload wrote,
# hands them to the native entry, and republishes findings as PostToolUse
# additionalContext. Its one invariant: it NEVER exits nonzero and a clean
# edit is silent.
#
# Tests:
#   T1  a Write that cuts a tracked test file   -> count + last line, exit 0
#   T2  originalFile baseline on an untracked   -> "before this edit"
#   T3  an Edit that renames heal               -> the patch line named
#   T4  an apply_patch stripping a .md newline  -> last line
#   T5  a Python syntax error                   -> "does not parse"
#   T6  new invalid JSON under fixtures/        -> silent
#   T7  a clean edit                            -> silent
#   T8  no binary answers (stub exits 2)        -> silent, exit 0

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
SHIM="${REPO_ROOT}/hooks/edit-integrity.sh"

[[ -f "$SHIM" ]] || { echo "FAIL: shim not found at $SHIM" >&2; exit 1; }
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

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  PASS: $*"; }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t edit-integrity-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# Pin the verified binary first on PATH so the suite tests the shim, not
# the operator's installed version (tests/hooks/test_king_delegation_guard.sh).
mkdir -p "$TMP/realbin"
ln -s "$BIN" "$TMP/realbin/fno-agents"

# The fixture repo: cli/src layout so the dotted module maps to
# fno.demo.helpers, and a test that still patches it by string.
REPO="$TMP/repo"
mkdir -p "$REPO/cli/src/fno/demo" "$REPO/cli/tests/unit" "$REPO/docs"
/usr/bin/git -C "$REPO" init -q
cat > "$REPO/cli/src/fno/demo/helpers.py" <<'PYEOF'
def heal():
    return 1

def keep():
    return 2
PYEOF
cat > "$REPO/cli/tests/unit/test_helpers.py" <<'PYEOF'
from fno.demo.helpers import keep

def test_one():
    patch("fno.demo.helpers.heal")

def test_two():
    assert keep() == 2

def test_three():
    assert True
PYEOF
printf 'hello\n' > "$REPO/docs/a.md"
/usr/bin/git -C "$REPO" add .
/usr/bin/git -C "$REPO" -c user.email=t@t -c user.name=t commit -qm base

run_shim() {
    (cd "$REPO" && PATH="$TMP/realbin:$PATH" bash "$SHIM")
}

payload() {
    python3 -c '
import json, sys
print(json.dumps(json.loads(sys.argv[1])))' "$1"
}

# T1: a Write that cuts the tracked test file: one test def destroyed
# mid-line and the trailing newline gone with it.
printf 'from fno.demo.helpers import keep\n\ndef test_one():\n    assert True\n\ndef test_two():\n    pass\ndef test' \
    > "$REPO/cli/tests/unit/test_helpers.py"
OUT="$(payload '{"tool_name": "Write", "cwd": "'"${REPO}"'", "tool_input": {"file_path": "'"${REPO}"'/cli/tests/unit/test_helpers.py"}}' | run_shim)"
RC=$?
if [[ $RC -eq 0 && "$OUT" == *"test count fell 3 -> 2 against HEAD"* \
    && "$OUT" == *"last line has no newline"* && "$OUT" == *additionalContext* ]]; then
  pass "T1 a cut Write reports the count drop and the newline, exit 0"
else
  fail "T1 rc=$RC out='$OUT'"
fi

# T2: originalFile carries the pre-edit text of an untracked file, so the
# baseline is the edit itself, not HEAD.
UNTRACKED="$REPO/cli/tests/unit/test_untracked.py"
printf 'def test_a():\n    pass\ndef test_b():\n    pass\n' > "$UNTRACKED"
OUT="$(payload '{"tool_name": "Write", "cwd": "'"${REPO}"'", "tool_input": {"file_path": "'"${UNTRACKED}"'"}, "tool_response": {"originalFile": "def test_a():\n    pass\ndef test_b():\n    pass\ndef test_c():\n    pass\n"}}' | run_shim)"
RC=$?
if [[ $RC -eq 0 && "$OUT" == *"test count fell 3 -> 2 against before this edit"* ]]; then
  pass "T2 the originalFile baseline names itself"
else
  fail "T2 rc=$RC out='$OUT'"
fi

# T2b: the pre-edit text ended with a newline and the edit dropped it; the
# baseline must keep that newline byte-exactly or the finding is silenced.
printf 'body' > "$REPO/docs/cut.md"
OUT="$(payload '{"tool_name": "Write", "cwd": "'"${REPO}"'", "tool_input": {"file_path": "'"${REPO}"'/docs/cut.md"}, "tool_response": {"originalFile": "body\n"}}' | run_shim)"
RC=$?
if [[ $RC -eq 0 && "$OUT" == *"docs/cut.md"*"last line has no newline and before this edit did"* ]]; then
  pass "T2b a dropped final newline survives the originalFile round-trip"
else
  fail "T2b rc=$RC out='$OUT'"
fi

# T3: an Edit renames top-level heal while the committed test still
# patches the dotted path; the context names that file and line.
/usr/bin/git -C "$REPO" checkout -- cli/tests/unit/test_helpers.py
cat > "$REPO/cli/src/fno/demo/helpers.py" <<'PYEOF'
def heal_now():
    return 1

def keep():
    return 2
PYEOF
OUT="$(payload '{"tool_name": "Edit", "cwd": "'"${REPO}"'", "tool_input": {"file_path": "'"${REPO}"'/cli/src/fno/demo/helpers.py"}}' | run_shim)"
RC=$?
if [[ $RC -eq 0 && "$OUT" == *"removed top-level heal"* \
    && "$OUT" == *"cli/tests/unit/test_helpers.py:4"* ]]; then
  pass "T3 a rename names the stale patch target"
else
  fail "T3 rc=$RC out='$OUT'"
fi
/usr/bin/git -C "$REPO" checkout -- cli/src/fno/demo/helpers.py

# T4: an apply_patch payload strips the .md final newline; the shim reads
# the header path from tool_input.command.
printf 'hello' > "$REPO/docs/a.md"
OUT="$(payload '{"tool_name": "apply_patch", "cwd": "'"${REPO}"'", "tool_input": {"command": "*** Begin Patch\n*** Update File: docs/a.md\n@@\n"}}' | run_shim)"
RC=$?
if [[ $RC -eq 0 && "$OUT" == *"docs/a.md"*"last line has no newline"* ]]; then
  pass "T4 an apply_patch payload names the header path"
else
  fail "T4 rc=$RC out='$OUT'"
fi
printf 'hello\n' > "$REPO/docs/a.md"

# T5: a Python edit that breaks the parse of a file HEAD parsed.
printf 'def (' > "$REPO/cli/src/fno/demo/helpers.py"
OUT="$(payload '{"tool_name": "Edit", "cwd": "'"${REPO}"'", "tool_input": {"file_path": "'"${REPO}"'/cli/src/fno/demo/helpers.py"}}' | run_shim)"
RC=$?
if [[ $RC -eq 0 && "$OUT" == *"does not parse"* ]]; then
  pass "T5 a broken parse is named, exit 0"
else
  fail "T5 rc=$RC out='$OUT'"
fi
/usr/bin/git -C "$REPO" checkout -- cli/src/fno/demo/helpers.py

# T6: a new invalid file under fixtures/ is a deliberate fixture; no finding.
mkdir -p "$REPO/fixtures"
printf '{\n' > "$REPO/fixtures/broken.json"
OUT="$(payload '{"tool_name": "Write", "cwd": "'"${REPO}"'", "tool_input": {"file_path": "'"${REPO}"'/fixtures/broken.json"}}' | run_shim)"
RC=$?
if [[ $RC -eq 0 && -z "$OUT" ]]; then
  pass "T6 a new invalid fixture file stays silent"
else
  fail "T6 rc=$RC out='$OUT'"
fi

# T7: a clean edit is silent.
printf 'hello there\n' > "$REPO/docs/a.md"
OUT="$(payload '{"tool_name": "Edit", "cwd": "'"${REPO}"'", "tool_input": {"file_path": "'"${REPO}"'/docs/a.md"}}' | run_shim)"
RC=$?
if [[ $RC -eq 0 && -z "$OUT" ]]; then
  pass "T7 a clean edit is silent, exit 0"
else
  fail "T7 rc=$RC out='$OUT'"
fi

# T8: no binary answers the entry - a stub on PATH exits 2, the env is
# unset, and $PWD holds no checkout; the shim says nothing and exits 0.
mkdir -p "$TMP/stubbin"
printf '#!/bin/sh\nexit 2\n' > "$TMP/stubbin/fno-agents"
chmod +x "$TMP/stubbin/fno-agents"
OUT="$(cd "$REPO" && env -u FNO_AGENTS_BIN PATH="$TMP/stubbin" /bin/bash "$SHIM" <<'EOFP'
{"tool_name": "Edit", "tool_input": {"file_path": "docs/a.md"}}
EOFP
)"
RC=$?
if [[ $RC -eq 0 && -z "$OUT" ]]; then
  pass "T8 no answering binary stays silent, exit 0"
else
  fail "T8 rc=$RC out='$OUT'"
fi

echo "[edit-integrity] $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
