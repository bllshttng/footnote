#!/usr/bin/env bash
# Cover the three dedup paths of scripts/memory/write-memory-entry.sh:
#   1. New entry (file written, MEMORY.md updated)
#   2. Identical match (no-op, rc=1, file unchanged)
#   3. Update path (different body, appends "Session ... update" stanza)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
WRITE="$REPO_ROOT/scripts/memory/write-memory-entry.sh"

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "  ok: $*" >&2; }

[[ -x "$WRITE" ]] || fail "write-memory-entry.sh not executable at $WRITE"

WORK=$(mktemp -d -t dedup-test-XXXXXX)

CAND_A='{"type":"project","name":"dedup_test_one","description":"dedup test fixture","body":"Body A.\n**Why:** original.\n**How to apply:** initial.\n"}'

# Case 1: new entry
bash "$WRITE" --memory-dir "$WORK" --session-id sid-001 --candidate "$CAND_A" \
    >/dev/null 2>&1 || fail "case1 write failed"
ENTRY="$WORK/project_dedup_test_one.md"
[[ -f "$ENTRY" ]] || fail "case1 entry not written"
grep -q '^auto_generated: true$' "$ENTRY" || fail "case1 auto_generated missing"
grep -q '^source_session: sid-001$' "$ENTRY" || fail "case1 source_session missing"
grep -q '^- \[dedup_test_one\]' "$WORK/MEMORY.md" || fail "case1 MEMORY.md not updated"
pass "case1 new entry"

# Snapshot for unchanged check
SNAP_FILE=$(stat -f '%m %z' "$ENTRY" 2>/dev/null || stat -c '%Y %s' "$ENTRY")
SNAP_INDEX=$(stat -f '%m %z' "$WORK/MEMORY.md" 2>/dev/null || stat -c '%Y %s' "$WORK/MEMORY.md")
sleep 1  # ensure mtime granularity wouldn't false-positive

# Case 2: identical match - rc=2 means "intentional dedup" (rc=1 is reserved
# for real errors like invalid JSON or missing args).
bash "$WRITE" --memory-dir "$WORK" --session-id sid-002 --candidate "$CAND_A" \
    >/dev/null 2>&1
RC=$?
[[ "$RC" == "2" ]] || fail "case2 expected rc=2 (dedup), got rc=$RC"
NOW_FILE=$(stat -f '%m %z' "$ENTRY" 2>/dev/null || stat -c '%Y %s' "$ENTRY")
NOW_INDEX=$(stat -f '%m %z' "$WORK/MEMORY.md" 2>/dev/null || stat -c '%Y %s' "$WORK/MEMORY.md")
[[ "$NOW_FILE" == "$SNAP_FILE" ]] || fail "case2 entry was modified by dedup hit"
[[ "$NOW_INDEX" == "$SNAP_INDEX" ]] || fail "case2 MEMORY.md was modified by dedup hit"
pass "case2 identical no-op"

# Case 3: update with different body. Requires --source-sha256 of the file
# as it stands on disk right now (read-before-write, x-8fc0) - the writer
# refuses (stages instead) an update without it.
CAND_B='{"type":"project","name":"dedup_test_one","description":"dedup test updated","body":"Body B.\n**Why:** revised.\n**How to apply:** new path.\n"}'
# A pipeline's exit status is the LAST command's (awk succeeds on empty
# input), so `A | awk || B | awk` never falls through when A is missing.
# Check which binary exists first instead.
if command -v sha256sum >/dev/null 2>&1; then
    SHA_BEFORE=$(sha256sum "$ENTRY" | awk '{print $1}')
else
    SHA_BEFORE=$(shasum -a 256 "$ENTRY" | awk '{print $1}')
fi
bash "$WRITE" --memory-dir "$WORK" --session-id sid-003 --candidate "$CAND_B" \
    --source-sha256 "$SHA_BEFORE" \
    >/dev/null 2>&1 || fail "case3 update failed"
grep -q '^Body A\.' "$ENTRY" || fail "case3 original body not preserved"
grep -q '^## Session sid-003 update$' "$ENTRY" || fail "case3 stanza missing"
grep -q '^Body B\.' "$ENTRY" || fail "case3 updated body not appended"
# MEMORY.md should still have ONE line for this entry.
LINE_COUNT=$(grep -c '^- \[dedup_test_one\]' "$WORK/MEMORY.md")
[[ "$LINE_COUNT" == "1" ]] || fail "case3 MEMORY.md duplicated: $LINE_COUNT lines"
pass "case3 update preserves original + appends stanza"

# Case 4: update WITHOUT --source-sha256 is staged, not written (x-8fc0
# read-before-write guard) - the live entry from case 3 must be untouched.
CAND_C='{"type":"project","name":"dedup_test_one","description":"dedup test updated again","body":"Body C.\n**Why:** no proof of read.\n**How to apply:** should be staged.\n"}'
PRE_CASE4=$(cat "$ENTRY")
RC=0
bash "$WRITE" --memory-dir "$WORK" --session-id sid-004 --candidate "$CAND_C" \
    >/dev/null 2>&1 || RC=$?
[[ "$RC" == "3" ]] || fail "case4 expected rc=3 (staged), got rc=$RC"
[[ "$(cat "$ENTRY")" == "$PRE_CASE4" ]] || fail "case4 live entry mutated despite missing --source-sha256"
[[ -f "$WORK/.staged/project_dedup_test_one.md" ]] || fail "case4 staged file not written"
pass "case4 update without --source-sha256 is staged, not written"

rm -rf "$WORK"
echo "PASS: dedup three-path coverage"
