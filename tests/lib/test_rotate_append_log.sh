#!/usr/bin/env bash
# Tests for scripts/lib/rotate-append-log.sh
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROTATE="$SCRIPT_DIR/../../scripts/lib/rotate-append-log.sh"
PASS=0
FAIL=0

if [[ ! -f "$ROTATE" ]]; then
    echo "FAIL: $ROTATE not found"
    exit 1
fi

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

# ---- AC2-HP: a log over its cap shrinks to <= cap with an intact last line ----
echo "AC2-HP: over-cap JSONL rotates, last line intact"
F=$(mktemp)
# 2000 JSON lines, each ~40 bytes -> ~80 KB; cap at 8 KB.
for i in $(seq 1 2000); do echo "{\"n\":$i,\"pad\":\"xxxxxxxxxxxxxxxxxxxx\"}"; done > "$F"
LAST_BEFORE=$(tail -1 "$F")
bash "$ROTATE" "$F" 8192
RC=$?
SIZE=$(wc -c < "$F" | tr -d '[:space:]')
if [[ $RC -eq 0 ]]; then pass "rc=0"; else fail "rc=$RC"; fi
if [[ "$SIZE" -le 8192 ]]; then pass "shrank to <= cap ($SIZE)"; else fail "size $SIZE > cap 8192"; fi
if [[ "$(tail -1 "$F")" == "$LAST_BEFORE" ]]; then pass "last line preserved"; else fail "last line changed"; fi
# every surviving line must be valid JSON (no partial leading line)
if python3 -c "import json,sys; [json.loads(l) for l in open(sys.argv[1]) if l.strip()]" "$F" 2>/dev/null; then
    pass "all surviving lines are valid JSON"
else
    fail "a surviving line is not valid JSON (partial line leaked)"
fi
rm -f "$F"

# ---- AC2-HP/idempotent: a log already under cap is untouched ----
echo "AC2: under-cap file untouched"
F=$(mktemp)
echo '{"small":true}' > "$F"
BEFORE=$(cat "$F")
bash "$ROTATE" "$F" 1048576
if [[ "$(cat "$F")" == "$BEFORE" ]]; then pass "under-cap file unchanged"; else fail "under-cap file modified"; fi
rm -f "$F"

# ---- AC2-ERR: missing path -> exit 0, file not created ----
echo "AC2-ERR: missing path is a clean no-op"
MISSING="/tmp/rotate-missing-$$-does-not-exist.jsonl"
rm -f "$MISSING"
bash "$ROTATE" "$MISSING" 1024
RC=$?
if [[ $RC -eq 0 ]]; then pass "rc=0 on missing path"; else fail "rc=$RC on missing path"; fi
if [[ ! -e "$MISSING" ]]; then pass "file not created"; else fail "file was created"; fi

# ---- AC2-EDGE: garbage cap -> no-op, no error ----
echo "AC2-EDGE: garbage cap degrades to no-op"
F=$(mktemp)
for i in $(seq 1 1000); do echo "line $i padding padding padding"; done > "$F"
BEFORE_SIZE=$(wc -c < "$F" | tr -d '[:space:]')
bash "$ROTATE" "$F" "not-a-number"
RC=$?
AFTER_SIZE=$(wc -c < "$F" | tr -d '[:space:]')
if [[ $RC -eq 0 ]]; then pass "rc=0 on garbage cap"; else fail "rc=$RC on garbage cap"; fi
if [[ "$AFTER_SIZE" == "$BEFORE_SIZE" ]]; then pass "file untouched on garbage cap"; else fail "file modified on garbage cap"; fi
# empty cap likewise
bash "$ROTATE" "$F" ""
if [[ $? -eq 0 ]]; then pass "rc=0 on empty cap"; else fail "nonzero on empty cap"; fi
rm -f "$F"

# ---- missing args -> usage error (exit 2) ----
echo "usage: missing args rejected"
bash "$ROTATE" >/dev/null 2>&1
if [[ $? -eq 2 ]]; then pass "rc=2 with no args"; else fail "expected rc=2 with no args"; fi

# ---- summary ----
TOTAL=$((PASS + FAIL))
echo
if [[ $FAIL -eq 0 ]]; then
    echo "PASS: rotate-append-log.sh tests ($PASS/$TOTAL)"
    exit 0
else
    echo "FAIL: rotate-append-log.sh tests ($PASS passed, $FAIL failed of $TOTAL)"
    exit 1
fi
