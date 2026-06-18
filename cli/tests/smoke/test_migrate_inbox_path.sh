#!/usr/bin/env bash
# Smoke tests for scripts/migrate-inbox-path.sh
# Roots are passed explicitly via FNO_INBOX_OLD_ROOT/FNO_INBOX_NEW_ROOT into a
# tempdir, so the tests never touch the real $HOME and carry no hardcoded vault.
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")/../../.." && pwd)/scripts/migrate-inbox-path.sh"

pass=0
fail=0

assert_eq() {
  local label="$1" expected="$2" actual="$3"
  if [[ "$actual" == "$expected" ]]; then
    echo "  PASS: $label"
    pass=$((pass + 1))
  else
    echo "  FAIL: $label"
    echo "    expected: $expected"
    echo "    actual:   $actual"
    fail=$((fail + 1))
  fi
}

assert_file_exists() {
  local label="$1" path="$2"
  if [[ -f "$path" ]]; then
    echo "  PASS: $label"
    pass=$((pass + 1))
  else
    echo "  FAIL: $label (file not found: $path)"
    fail=$((fail + 1))
  fi
}

assert_file_absent() {
  local label="$1" path="$2"
  if [[ ! -e "$path" ]]; then
    echo "  PASS: $label"
    pass=$((pass + 1))
  else
    echo "  FAIL: $label (file still exists: $path)"
    fail=$((fail + 1))
  fi
}

assert_rc() {
  local label="$1" expected_rc="$2" actual_rc="$3"
  if [[ "$actual_rc" == "$expected_rc" ]]; then
    echo "  PASS: $label (rc=$actual_rc)"
    pass=$((pass + 1))
  else
    echo "  FAIL: $label (expected rc=$expected_rc, got rc=$actual_rc)"
    fail=$((fail + 1))
  fi
}

# ---------------------------------------------------------------------------
# AC1-HP: Empty old root (no .md files, only empty archive/) is cleaned up
# ---------------------------------------------------------------------------
echo ""
echo "AC1-HP: empty old root migrates cleanly"
tmp=$(mktemp -d)
OLD="$tmp/old-inbox/agents/inbox"
NEW="$tmp/threaded/internal/agents"
mkdir -p "$OLD/archive"

output=$(FNO_INBOX_OLD_ROOT="$OLD" FNO_INBOX_NEW_ROOT="$NEW" bash "$SCRIPT" 2>&1)
rc=$?

assert_rc "AC1-HP exits 0" 0 "$rc"
assert_file_absent "AC1-HP old root removed" "$OLD"
# No files should appear in new path
new_count=$(find "$NEW" -name "*.md" 2>/dev/null | wc -l | tr -d ' ')
assert_eq "AC1-HP no new files created" "0" "$new_count"
rm -rf "$tmp"

# ---------------------------------------------------------------------------
# AC2-ERR: Idempotent - second run prints "already migrated" and exits 0
# ---------------------------------------------------------------------------
echo ""
echo "AC2-ERR: idempotent - second run is safe"
tmp=$(mktemp -d)
OLD="$tmp/old-inbox/agents/inbox"
NEW="$tmp/threaded/internal/agents"
mkdir -p "$OLD"

# First run
FNO_INBOX_OLD_ROOT="$OLD" FNO_INBOX_NEW_ROOT="$NEW" bash "$SCRIPT" > /dev/null 2>&1
first_rc=$?
assert_rc "AC2-ERR first run exits 0" 0 "$first_rc"

# Second run - old root should be gone, so "already migrated"
output2=$(FNO_INBOX_OLD_ROOT="$OLD" FNO_INBOX_NEW_ROOT="$NEW" bash "$SCRIPT" 2>&1)
second_rc=$?

assert_rc "AC2-ERR second run exits 0" 0 "$second_rc"
if echo "$output2" | grep -q "already migrated"; then
  echo "  PASS: AC2-ERR second run prints 'already migrated'"
  ((pass++))
else
  echo "  FAIL: AC2-ERR expected 'already migrated' in output, got: $output2"
  ((fail++))
fi
rm -rf "$tmp"

# ---------------------------------------------------------------------------
# AC4-EDGE: foo.md migrates to foo/inbox.md with byte-identical content
# ---------------------------------------------------------------------------
echo ""
echo "AC4-EDGE: existing inbox file migrates to new location"
tmp=$(mktemp -d)
OLD="$tmp/old-inbox/agents/inbox"
NEW="$tmp/threaded/internal/agents"
mkdir -p "$OLD"
echo "hello from foo project" > "$OLD/foo.md"

output=$(FNO_INBOX_OLD_ROOT="$OLD" FNO_INBOX_NEW_ROOT="$NEW" bash "$SCRIPT" 2>&1)
rc=$?

assert_rc "AC4-EDGE exits 0" 0 "$rc"

NEW_FILE="$NEW/foo/inbox.md"
assert_file_exists "AC4-EDGE new file exists" "$NEW_FILE"
assert_file_absent "AC4-EDGE source file removed" "$OLD/foo.md"

# Byte-identical content check
if [[ -f "$NEW_FILE" ]]; then
  orig_content="hello from foo project"
  new_content=$(cat "$NEW_FILE")
  assert_eq "AC4-EDGE content matches" "$orig_content" "$new_content"
fi
rm -rf "$tmp"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "Results: $pass passed, $fail failed"
if [[ $fail -gt 0 ]]; then
  exit 1
fi
exit 0
