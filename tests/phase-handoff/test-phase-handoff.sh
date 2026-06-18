#!/usr/bin/env bash
# Test suite for scripts/lib/phase-handoff.sh (Story 2)
# TDD: Run BEFORE implementing to get RED, then implement to get GREEN

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HANDOFF_LIB="$REPO_ROOT/scripts/lib/phase-handoff.sh"

PASS=0
FAIL=0

pass() { echo "  PASS: $*"; ((PASS++)) || true; }
fail() { echo "  FAIL: $*"; ((FAIL++)) || true; }

# Create a clean tmp workspace per test run
TMPDIR_BASE="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_BASE"' EXIT

cd "$TMPDIR_BASE"
mkdir -p .fno/artifacts

echo "=== Story 2: phase-handoff.sh helper tests ==="

# --- Prerequisite: library exists ---
echo ""
echo "--- Library exists ---"
if [[ -f "$HANDOFF_LIB" ]]; then
  pass "phase-handoff.sh exists at $HANDOFF_LIB"
else
  fail "phase-handoff.sh missing at $HANDOFF_LIB"
  echo "=== Results: $PASS passed, $FAIL failed ==="
  exit 1
fi

# Source the library
# shellcheck source=/dev/null
source "$HANDOFF_LIB"

# ---------------------------------------------------------------
# AC1-HP: ph_write produces a valid yaml file at the expected path
# ---------------------------------------------------------------
echo ""
echo "--- AC1-HP: ph_write creates artifact at expected path ---"

WORK_DIR="$TMPDIR_BASE/test-write"
mkdir -p "$WORK_DIR/.fno/artifacts"
cd "$WORK_DIR"

PH_ARTIFACTS_DIR=".fno/artifacts/handoff"
ph_write "do" "sess001" "stories_completed: [1, 2, 3]" 2>/dev/null
EXPECTED_PATH="$PH_ARTIFACTS_DIR/do-sess001.md"
if [[ -f "$EXPECTED_PATH" ]]; then
  pass "AC1-HP: artifact created at $EXPECTED_PATH"
else
  fail "AC1-HP: artifact missing at $EXPECTED_PATH"
fi

# File should contain YAML frontmatter with phase and session_id
if grep -q "^phase: do" "$EXPECTED_PATH" 2>/dev/null; then
  pass "AC1-HP: artifact contains phase: do"
else
  fail "AC1-HP: artifact missing 'phase: do' in frontmatter"
fi

if grep -q "^session_id: sess001" "$EXPECTED_PATH" 2>/dev/null; then
  pass "AC1-HP: artifact contains session_id: sess001"
else
  fail "AC1-HP: artifact missing 'session_id: sess001' in frontmatter"
fi

if grep -q "stories_completed" "$EXPECTED_PATH" 2>/dev/null; then
  pass "AC1-HP: artifact contains custom payload field"
else
  fail "AC1-HP: artifact missing custom payload field 'stories_completed'"
fi

# ph_write should return exit 0 on success
cd "$TMPDIR_BASE"
mkdir -p "test-exit/.fno/artifacts"
cd "test-exit"
if ph_write "think" "sess-exit" "key: val" 2>/dev/null; then
  pass "AC1-HP: ph_write returns exit 0 on success"
else
  fail "AC1-HP: ph_write returned non-zero on success"
fi

# ---------------------------------------------------------------
# AC2-ERR: oversized payload is truncated with marker
# ---------------------------------------------------------------
echo ""
echo "--- AC2-ERR: oversized payload gets truncated ---"

cd "$TMPDIR_BASE"
mkdir -p "test-trunc/.fno/artifacts"
cd "test-trunc"

# Generate a payload well over 2000 chars
LONG_VAL=$(python3 -c "print('x' * 3000)")
ph_write "think" "sess-trunc" "summary: $LONG_VAL" 2>/dev/null

ARTIFACT=".fno/artifacts/handoff/think-sess-trunc.md"
if [[ -f "$ARTIFACT" ]]; then
  pass "AC2-ERR: oversized artifact still created (truncated)"
  FILE_SIZE=$(wc -c < "$ARTIFACT")
  if [[ "$FILE_SIZE" -lt 2500 ]]; then
    pass "AC2-ERR: artifact is under 2500 bytes (was $FILE_SIZE)"
  else
    fail "AC2-ERR: artifact too large: $FILE_SIZE bytes (expected < 2500)"
  fi
  if grep -q "truncated" "$ARTIFACT"; then
    pass "AC2-ERR: artifact contains truncation marker"
  else
    fail "AC2-ERR: artifact missing truncation marker"
  fi
else
  fail "AC2-ERR: truncated artifact was not created"
fi

# ---------------------------------------------------------------
# AC3-UI: helpers print confirmation to stderr, not stdout
# ---------------------------------------------------------------
echo ""
echo "--- AC3-UI: confirmations go to stderr, stdout stays clean ---"

cd "$TMPDIR_BASE"
mkdir -p "test-stderr/.fno/artifacts"
cd "test-stderr"

STDOUT_ONLY=$(ph_write "plan" "sess-stderr" "plan_path: foo" 2>/dev/null)
if [[ -z "$STDOUT_ONLY" ]]; then
  pass "AC3-UI: ph_write produces no stdout output"
else
  fail "AC3-UI: ph_write wrote to stdout: '$STDOUT_ONLY'"
fi

# ph_read should output to stdout
ph_write "plan" "sess-read-test" "scope_classification: feature" 2>/dev/null
READ_OUT=$(ph_read "plan" "sess-read-test" 2>/dev/null)
if [[ -n "$READ_OUT" ]]; then
  pass "AC3-UI: ph_read outputs to stdout"
else
  fail "AC3-UI: ph_read produced no stdout output"
fi

# ph_read output should be parseable JSON (or at least contain the field)
if echo "$READ_OUT" | grep -q "scope_classification"; then
  pass "AC3-UI: ph_read stdout contains payload field"
else
  fail "AC3-UI: ph_read stdout missing payload field. Got: $READ_OUT"
fi

# ---------------------------------------------------------------
# AC4-EDGE: ph_write refuses to overwrite existing artifact
# ---------------------------------------------------------------
echo ""
echo "--- AC4-EDGE: refuse overwrite of existing artifact ---"

cd "$TMPDIR_BASE"
mkdir -p "test-overwrite/.fno/artifacts"
cd "test-overwrite"

ph_write "do" "sess-ow" "stories_completed: [1]" 2>/dev/null

# Second write to same phase+session should fail
if ph_write "do" "sess-ow" "stories_completed: [2]" 2>/dev/null; then
  fail "AC4-EDGE: ph_write should refuse to overwrite existing artifact"
else
  pass "AC4-EDGE: ph_write returned non-zero on overwrite attempt"
fi

# Content should remain from first write
if grep -q "stories_completed: \[1\]" ".fno/artifacts/handoff/do-sess-ow.md" 2>/dev/null; then
  pass "AC4-EDGE: original content preserved after refused overwrite"
else
  fail "AC4-EDGE: original content was modified or missing"
fi

# ---------------------------------------------------------------
# ph_read_latest: reads from the most recent session
# ---------------------------------------------------------------
echo ""
echo "--- ph_read_latest: returns most recent session artifact ---"

cd "$TMPDIR_BASE"
mkdir -p "test-latest/.fno/artifacts"
cd "test-latest"

# Write two think artifacts with different sessions; sleep 1s so mtime differs
ph_write "think" "sessA" "key: older" 2>/dev/null
sleep 1
ph_write "think" "sessB" "key: newer" 2>/dev/null

LATEST=$(ph_read_latest "think" 2>/dev/null)
if echo "$LATEST" | grep -q "newer"; then
  pass "ph_read_latest: returns content from most recent session"
else
  fail "ph_read_latest: expected 'newer', got: $LATEST"
fi

# ---------------------------------------------------------------
# ph_list: lists all phase artifacts for a session
# ---------------------------------------------------------------
echo ""
echo "--- ph_list: lists artifacts for a session ---"

cd "$TMPDIR_BASE"
mkdir -p "test-list/.fno/artifacts"
cd "test-list"

ph_write "think" "sess-list" "key: a" 2>/dev/null
ph_write "plan"  "sess-list" "key: b" 2>/dev/null
ph_write "do"    "sess-list" "key: c" 2>/dev/null

LIST_OUT=$(ph_list "sess-list" 2>/dev/null)
if echo "$LIST_OUT" | grep -q "think-sess-list"; then
  pass "ph_list: lists think artifact"
else
  fail "ph_list: missing think artifact. Got: $LIST_OUT"
fi
if echo "$LIST_OUT" | grep -q "plan-sess-list"; then
  pass "ph_list: lists plan artifact"
else
  fail "ph_list: missing plan artifact. Got: $LIST_OUT"
fi
if echo "$LIST_OUT" | grep -q "do-sess-list"; then
  pass "ph_list: lists do artifact"
else
  fail "ph_list: missing do artifact. Got: $LIST_OUT"
fi

# ph_list for unknown session should produce empty output (not an error)
EMPTY_LIST=$(ph_list "sess-nonexistent" 2>/dev/null)
if [[ -z "$EMPTY_LIST" ]]; then
  pass "ph_list: empty output for nonexistent session"
else
  fail "ph_list: expected empty output for nonexistent session, got: $EMPTY_LIST"
fi

# ---------------------------------------------------------------
# ph_read: nonexistent artifact returns non-zero
# ---------------------------------------------------------------
echo ""
echo "--- ph_read: nonexistent artifact returns error ---"

cd "$TMPDIR_BASE"
mkdir -p "test-no-read/.fno/artifacts"
cd "test-no-read"

if ph_read "do" "no-such-session" 2>/dev/null; then
  fail "ph_read: should return non-zero for missing artifact"
else
  pass "ph_read: returns non-zero for missing artifact"
fi

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]] || exit 1
