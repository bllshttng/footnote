#!/usr/bin/env bash
# test-stamp-plan.sh -- test suite for the in-package fno.plan._stamp module
# (was scripts/lib/stamp-plan.py).
#
# Each AC block is clearly labeled. Tests run TDD-style:
# all ACs are defined here; they fail before implementation, pass after.
#
# Usage: bash tests/test-stamp-plan.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# The stamper is now an in-package module run via `python3 -m fno.plan._stamp`.
# Put cli/src on PYTHONPATH so the module resolves when running from the repo.
export PYTHONPATH="$REPO_ROOT/cli/src${PYTHONPATH:+:$PYTHONPATH}"
STAMP_PY=(python3 -m fno.plan._stamp)
TMP=$(mktemp -d -t stamp-plan-test.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

make_folder_plan() {
    # $1 = dir name under $TMP; $2 optional extra frontmatter lines
    local dir="$TMP/$1"
    mkdir -p "$dir"
    local extra="${2:-}"
    cat > "$dir/00-INDEX.md" <<EOF
---
created: 2026-04-21T10:00:00Z
scope: test-scope
${extra}---

# Test Plan
EOF
    echo "$dir"
}

make_quick_plan() {
    # $1 = file name under $TMP (e.g. quick.md)
    local fpath="$TMP/$1"
    cat > "$fpath" <<EOF
---
created: 2026-04-21T10:00:00Z
scope: quick-scope
---

# Quick Plan
EOF
    echo "$fpath"
}

frontmatter_value() {
    # Extract a scalar value from frontmatter: frontmatter_value <file> <key>
    python3 - "$1" "$2" <<'PYEOF'
import sys, re
path, key = sys.argv[1], sys.argv[2]
with open(path) as f:
    content = f.read()
m = re.search(r'^---\n(.*?)\n---', content, re.DOTALL)
if not m:
    sys.exit(1)
for line in m.group(1).splitlines():
    if line.startswith(key + ':'):
        print(line[len(key)+1:].strip())
        sys.exit(0)
sys.exit(1)
PYEOF
}

frontmatter_list() {
    # Extract a flat list value: frontmatter_list <file> <key>
    python3 - "$1" "$2" <<'PYEOF'
import sys, re
path, key = sys.argv[1], sys.argv[2]
with open(path) as f:
    content = f.read()
m = re.search(r'^---\n(.*?)\n---', content, re.DOTALL)
if not m:
    sys.exit(1)
# inline list format: key: [val1, val2]
for line in m.group(1).splitlines():
    if line.startswith(key + ':'):
        print(line[len(key)+1:].strip())
        sys.exit(0)
sys.exit(1)
PYEOF
}

# ---------------------------------------------------------------------------
# Task 1.1: Stamp a folder plan with no prior stamp (AC1-HP)
# ---------------------------------------------------------------------------
echo ""
echo "=== Task 1.1 ==="

echo "--- AC1-HP: Stamp folder plan with no prior stamp ---"
PLAN_DIR=$(make_folder_plan "plan-ac1hp")
"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_DIR" \
    --session-id SID1 \
    --url https://github.com/org/repo/pull/1
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "AC1-HP: exit 0" || fail "AC1-HP: expected exit 0, got $EXIT"

IDX="$PLAN_DIR/00-INDEX.md"
STATUS=$(frontmatter_value "$IDX" "status" 2>/dev/null)
[[ "$STATUS" == "shipped" ]] && pass "AC1-HP: status=shipped" || fail "AC1-HP: status expected 'shipped', got '$STATUS'"

SHIPPED_AT=$(frontmatter_value "$IDX" "shipped_at" 2>/dev/null)
[[ -n "$SHIPPED_AT" ]] && pass "AC1-HP: shipped_at present" || fail "AC1-HP: shipped_at missing"

URLS=$(frontmatter_list "$IDX" "urls" 2>/dev/null)
[[ "$URLS" == *"https://github.com/org/repo/pull/1"* ]] && pass "AC1-HP: urls contains URL1" || fail "AC1-HP: urls missing URL1 (got '$URLS')"

SIDS=$(frontmatter_list "$IDX" "session_ids" 2>/dev/null)
[[ "$SIDS" == *"SID1"* ]] && pass "AC1-HP: session_ids contains SID1" || fail "AC1-HP: session_ids missing SID1 (got '$SIDS')"

# Original fields preserved byte-for-byte
CREATED=$(frontmatter_value "$IDX" "created" 2>/dev/null)
[[ "$CREATED" == "2026-04-21T10:00:00Z" ]] && pass "AC1-HP: created field preserved" || fail "AC1-HP: created field corrupted (got '$CREATED')"

SCOPE=$(frontmatter_value "$IDX" "scope" 2>/dev/null)
[[ "$SCOPE" == "test-scope" ]] && pass "AC1-HP: scope field preserved" || fail "AC1-HP: scope field corrupted (got '$SCOPE')"

# COMPLETION.md created for folder plans (Task 1.3 full content tested later)
[[ -f "$PLAN_DIR/COMPLETION.md" ]] && pass "AC1-HP: COMPLETION.md written" || fail "AC1-HP: COMPLETION.md missing"

# ---------------------------------------------------------------------------
# Task 1.1: Invalid plan path (AC2-ERR)
# ---------------------------------------------------------------------------
echo ""
echo "--- AC2-ERR: Invalid plan path ---"

# Non-existent path
"${STAMP_PY[@]}" stamp --plan-path "$TMP/no-such-path" --session-id SID1 --url URL1 2>/dev/null
EXIT=$?
[[ $EXIT -ne 0 ]] && pass "AC2-ERR: non-existent path exits non-zero" || fail "AC2-ERR: non-existent path should exit non-zero"

# Folder without 00-INDEX.md
NO_IDX="$TMP/no-index-dir"
mkdir -p "$NO_IDX"
"${STAMP_PY[@]}" stamp --plan-path "$NO_IDX" --session-id SID1 --url URL1 2>/dev/null
EXIT=$?
[[ $EXIT -ne 0 ]] && pass "AC2-ERR: folder without 00-INDEX.md exits non-zero" || fail "AC2-ERR: folder without 00-INDEX.md should exit non-zero"
# No files created
[[ ! -f "$NO_IDX/00-INDEX.md" ]] && pass "AC2-ERR: no file created on error" || fail "AC2-ERR: file was created unexpectedly"

# Stderr message for missing folder
ERR_MSG=$("${STAMP_PY[@]}" stamp --plan-path "$TMP/missing" --session-id SID1 --url URL1 2>&1 >/dev/null)
[[ -n "$ERR_MSG" ]] && pass "AC2-ERR: stderr message on missing path" || fail "AC2-ERR: no stderr message"

# ---------------------------------------------------------------------------
# Task 1.1: Quick (single-file) plan (AC4-EDGE)
# ---------------------------------------------------------------------------
echo ""
echo "--- AC4-EDGE: Quick (single-file) plan ---"
QUICK=$(make_quick_plan "quick.md")
"${STAMP_PY[@]}" stamp \
    --plan-path "$QUICK" \
    --session-id SID_Q \
    --url https://github.com/org/repo/pull/99
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "AC4-EDGE: quick plan exit 0" || fail "AC4-EDGE: quick plan expected exit 0, got $EXIT"

STATUS_Q=$(frontmatter_value "$QUICK" "status" 2>/dev/null)
[[ "$STATUS_Q" == "shipped" ]] && pass "AC4-EDGE: quick plan status=shipped" || fail "AC4-EDGE: quick plan status expected 'shipped', got '$STATUS_Q'"

SIBLING_COMPLETION="$(dirname "$QUICK")/COMPLETION.md"
[[ ! -f "$SIBLING_COMPLETION" ]] && pass "AC4-EDGE: no COMPLETION.md sibling for quick plan" || fail "AC4-EDGE: COMPLETION.md should not be created for quick plans"

# ---------------------------------------------------------------------------
# Task 1.2: Idempotent re-stamp (AC1-HP)
# ---------------------------------------------------------------------------
echo ""
echo "=== Task 1.2 ==="

echo "--- 1.2-AC1-HP: Idempotent re-stamp ---"
PLAN_IDEM=$(make_folder_plan "plan-idem")
# First stamp
"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_IDEM" \
    --session-id SID1 \
    --url https://github.com/org/repo/pull/10
IDX_IDEM="$PLAN_IDEM/00-INDEX.md"
SHIPPED_AT_FIRST=$(frontmatter_value "$IDX_IDEM" "shipped_at" 2>/dev/null)
CONTENT_BEFORE=$(cat "$IDX_IDEM")

# Second stamp with same args
"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_IDEM" \
    --session-id SID1 \
    --url https://github.com/org/repo/pull/10
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "1.2-AC1-HP: re-stamp exits 0" || fail "1.2-AC1-HP: re-stamp should exit 0, got $EXIT"

CONTENT_AFTER=$(cat "$IDX_IDEM")
[[ "$CONTENT_BEFORE" == "$CONTENT_AFTER" ]] && pass "1.2-AC1-HP: file unchanged on re-stamp" || fail "1.2-AC1-HP: file was modified on re-stamp"

SHIPPED_AT_SECOND=$(frontmatter_value "$IDX_IDEM" "shipped_at" 2>/dev/null)
[[ "$SHIPPED_AT_FIRST" == "$SHIPPED_AT_SECOND" ]] && pass "1.2-AC1-HP: shipped_at not rewritten" || fail "1.2-AC1-HP: shipped_at changed on re-stamp"

URLS_IDEM=$(frontmatter_list "$IDX_IDEM" "urls" 2>/dev/null)
# Count occurrences of the URL - should appear exactly once
URL_COUNT=$(echo "$URLS_IDEM" | grep -o "pull/10" | wc -l | tr -d ' ')
[[ "$URL_COUNT" -eq 1 ]] && pass "1.2-AC1-HP: no duplicate URL" || fail "1.2-AC1-HP: URL duplicated (count=$URL_COUNT)"

# ---------------------------------------------------------------------------
# Task 1.2: Accumulate across ships (AC2-HP)
# ---------------------------------------------------------------------------
echo ""
echo "--- 1.2-AC2-HP: Accumulate across ships ---"
PLAN_ACCUM=$(make_folder_plan "plan-accum")
IDX_ACCUM="$PLAN_ACCUM/00-INDEX.md"

"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_ACCUM" \
    --session-id SID1 \
    --url https://github.com/org/repo/pull/20

FIRST_AT=$(frontmatter_value "$IDX_ACCUM" "shipped_at" 2>/dev/null)

"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_ACCUM" \
    --session-id SID2 \
    --url https://github.com/org/repo2/pull/21 \
    --expected-url-count 2
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "1.2-AC2-HP: second stamp exits 0" || fail "1.2-AC2-HP: second stamp expected exit 0, got $EXIT"

URLS_ACCUM=$(frontmatter_list "$IDX_ACCUM" "urls" 2>/dev/null)
[[ "$URLS_ACCUM" == *"pull/20"* ]] && pass "1.2-AC2-HP: URL1 preserved" || fail "1.2-AC2-HP: URL1 missing"
[[ "$URLS_ACCUM" == *"pull/21"* ]] && pass "1.2-AC2-HP: URL2 added" || fail "1.2-AC2-HP: URL2 missing"

SIDS_ACCUM=$(frontmatter_list "$IDX_ACCUM" "session_ids" 2>/dev/null)
[[ "$SIDS_ACCUM" == *"SID1"* ]] && pass "1.2-AC2-HP: SID1 preserved" || fail "1.2-AC2-HP: SID1 missing"
[[ "$SIDS_ACCUM" == *"SID2"* ]] && pass "1.2-AC2-HP: SID2 added" || fail "1.2-AC2-HP: SID2 added"

STATUS_ACCUM=$(frontmatter_value "$IDX_ACCUM" "status" 2>/dev/null)
[[ "$STATUS_ACCUM" == "shipped" ]] && pass "1.2-AC2-HP: status still shipped (not done)" || fail "1.2-AC2-HP: status expected 'shipped', got '$STATUS_ACCUM'"

SECOND_AT=$(frontmatter_value "$IDX_ACCUM" "shipped_at" 2>/dev/null)
[[ "$FIRST_AT" == "$SECOND_AT" ]] && pass "1.2-AC2-HP: shipped_at unchanged on second stamp" || fail "1.2-AC2-HP: shipped_at changed on second stamp"

# ---------------------------------------------------------------------------
# Task 1.2: Graduate to done (AC3-HP)
# ---------------------------------------------------------------------------
echo ""
echo "--- 1.2-AC3-HP: Graduate to done ---"
PLAN_GRAD=$(make_folder_plan "plan-grad")
IDX_GRAD="$PLAN_GRAD/00-INDEX.md"

"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_GRAD" \
    --session-id SID1 \
    --url https://github.com/org/repo/pull/30 \
    --expected-url-count 2

"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_GRAD" \
    --session-id SID2 \
    --url https://github.com/org/repo2/pull/31 \
    --expected-url-count 2

"${STAMP_PY[@]}" graduate --plan-path "$PLAN_GRAD"
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "1.2-AC3-HP: graduate exits 0" || fail "1.2-AC3-HP: graduate expected exit 0, got $EXIT"

STATUS_GRAD=$(frontmatter_value "$IDX_GRAD" "status" 2>/dev/null)
[[ "$STATUS_GRAD" == "done" ]] && pass "1.2-AC3-HP: status=done after graduate" || fail "1.2-AC3-HP: status expected 'done', got '$STATUS_GRAD'"

SCOPE_GRAD=$(frontmatter_value "$IDX_GRAD" "scope" 2>/dev/null)
[[ "$SCOPE_GRAD" == "test-scope" ]] && pass "1.2-AC3-HP: scope preserved after graduate" || fail "1.2-AC3-HP: scope corrupted after graduate"

# ---------------------------------------------------------------------------
# Task 1.2: Graduate when insufficient URLs (AC4-EDGE)
# ---------------------------------------------------------------------------
echo ""
echo "--- 1.2-AC4-EDGE: Graduate with insufficient URLs ---"
PLAN_INSUF=$(make_folder_plan "plan-insuf")
IDX_INSUF="$PLAN_INSUF/00-INDEX.md"

"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_INSUF" \
    --session-id SID1 \
    --url https://github.com/org/repo/pull/40 \
    --expected-url-count 2

"${STAMP_PY[@]}" graduate --plan-path "$PLAN_INSUF"
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "1.2-AC4-EDGE: graduate exits 0 when insufficient" || fail "1.2-AC4-EDGE: expected exit 0, got $EXIT"

STATUS_INSUF=$(frontmatter_value "$IDX_INSUF" "status" 2>/dev/null)
[[ "$STATUS_INSUF" == "shipped" ]] && pass "1.2-AC4-EDGE: status stays shipped" || fail "1.2-AC4-EDGE: status expected 'shipped', got '$STATUS_INSUF'"

# ---------------------------------------------------------------------------
# Task 1.2: Malformed frontmatter (AC5-ERR)
# ---------------------------------------------------------------------------
echo ""
echo "--- 1.2-AC5-ERR: Malformed frontmatter ---"
MALFORMED="$TMP/malformed.md"
cat > "$MALFORMED" <<'EOF'
---
created: 2026-04-21T10:00:00Z
nested:
  key: value
scope: test
---

# Malformed Plan
EOF

ERR_OUT=$("${STAMP_PY[@]}" stamp --plan-path "$MALFORMED" --session-id SID1 --url URL1 2>&1)
EXIT=$?
[[ $EXIT -ne 0 ]] && pass "1.2-AC5-ERR: malformed frontmatter exits non-zero" || fail "1.2-AC5-ERR: expected non-zero exit"
[[ -n "$ERR_OUT" ]] && pass "1.2-AC5-ERR: error message emitted" || fail "1.2-AC5-ERR: no error message"
CONTENT_AFTER=$(cat "$MALFORMED")
# File should not be modified - check it still has the nested: block
[[ "$CONTENT_AFTER" == *"nested:"* ]] && pass "1.2-AC5-ERR: file not modified on parse error" || fail "1.2-AC5-ERR: file was modified despite parse error"

# ---------------------------------------------------------------------------
# Task 1.3: COMPLETION.md first ship (AC1-HP)
# ---------------------------------------------------------------------------
echo ""
echo "=== Task 1.3 ==="

echo "--- 1.3-AC1-HP: First ship writes COMPLETION.md ---"
PLAN_C1=$(make_folder_plan "plan-completion1")
"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_C1" \
    --session-id SID_C1 \
    --url https://github.com/org/repo/pull/50 \
    --completion-note "Shipped feature X. Works well."
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "1.3-AC1-HP: exit 0" || fail "1.3-AC1-HP: expected exit 0, got $EXIT"

COMP="$PLAN_C1/COMPLETION.md"
[[ -f "$COMP" ]] && pass "1.3-AC1-HP: COMPLETION.md created" || fail "1.3-AC1-HP: COMPLETION.md missing"
[[ -f "$COMP" ]] && grep -q "## Ship 1" "$COMP" && pass "1.3-AC1-HP: Ship 1 section present" || fail "1.3-AC1-HP: Ship 1 section missing"
[[ -f "$COMP" ]] && grep -q "SID_C1" "$COMP" && pass "1.3-AC1-HP: session ID in COMPLETION.md" || fail "1.3-AC1-HP: session ID missing"
[[ -f "$COMP" ]] && grep -q "pull/50" "$COMP" && pass "1.3-AC1-HP: URL in COMPLETION.md" || fail "1.3-AC1-HP: URL missing"
[[ -f "$COMP" ]] && grep -q "Shipped feature X" "$COMP" && pass "1.3-AC1-HP: completion note in COMPLETION.md" || fail "1.3-AC1-HP: completion note missing"

# ---------------------------------------------------------------------------
# Task 1.3: Second ship appends (AC2-HP)
# ---------------------------------------------------------------------------
echo ""
echo "--- 1.3-AC2-HP: Second ship appends ---"
"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_C1" \
    --session-id SID_C2 \
    --url https://github.com/org/repo2/pull/51 \
    --completion-note "Follow-up fix."
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "1.3-AC2-HP: second stamp exit 0" || fail "1.3-AC2-HP: expected exit 0, got $EXIT"

COMP2="$PLAN_C1/COMPLETION.md"
[[ -f "$COMP2" ]] && grep -q "## Ship 1" "$COMP2" && pass "1.3-AC2-HP: Ship 1 still present" || fail "1.3-AC2-HP: Ship 1 was overwritten"
[[ -f "$COMP2" ]] && grep -q "## Ship 2" "$COMP2" && pass "1.3-AC2-HP: Ship 2 appended" || fail "1.3-AC2-HP: Ship 2 missing"
[[ -f "$COMP2" ]] && grep -q "Follow-up fix" "$COMP2" && pass "1.3-AC2-HP: second note present" || fail "1.3-AC2-HP: second note missing"
# Ship 1 section unchanged - original note still there
[[ -f "$COMP2" ]] && grep -q "Shipped feature X" "$COMP2" && pass "1.3-AC2-HP: Ship 1 note unchanged" || fail "1.3-AC2-HP: Ship 1 note corrupted"

# ---------------------------------------------------------------------------
# Task 1.3: Quick plan skips COMPLETION.md (AC4-EDGE)
# ---------------------------------------------------------------------------
echo ""
echo "--- 1.3-AC4-EDGE: Quick plan skips COMPLETION.md ---"
QUICK2=$(make_quick_plan "quick2.md")
"${STAMP_PY[@]}" stamp \
    --plan-path "$QUICK2" \
    --session-id SID_QC \
    --url https://github.com/org/repo/pull/99 \
    --completion-note "Quick note."
SIBLING_DIR="$(dirname "$QUICK2")"
[[ ! -f "$SIBLING_DIR/COMPLETION.md" ]] && pass "1.3-AC4-EDGE: no COMPLETION.md for quick plan" || fail "1.3-AC4-EDGE: COMPLETION.md must not be written for quick plans"

# ---------------------------------------------------------------------------
# Review regression: parser must skip comment lines in frontmatter
# Gemini PR #159 review flagged that template frontmatter with commented
# examples (e.g., `# linear: {TEAM}-XXX`) would crash the parser.
# ---------------------------------------------------------------------------
echo ""
echo "--- REGRESSION: comment lines in frontmatter must not crash parser ---"
COMMENT_PLAN="$TMP/plan-comments"
mkdir -p "$COMMENT_PLAN"
cat > "$COMMENT_PLAN/00-INDEX.md" <<EOF
---
created: 2026-04-22
scope: feature
# This is a top-level comment and must not crash the parser
# linear: {TEAM}-XXX              # Only if config.linear.enabled
# depends_on:                     # Optional block comment
#   - ../sibling-plan-slug        # indented comment - also safe
---

# Plan body
EOF
"${STAMP_PY[@]}" stamp \
    --plan-path "$COMMENT_PLAN" \
    --session-id SID_COMMENTS \
    --url https://github.com/org/repo/pull/999 \
    --completion-note "regression: commented frontmatter"
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "REGRESSION: comment-line frontmatter stamps cleanly" \
    || fail "REGRESSION: parser still crashes on comments (exit=$EXIT)"
grep -q "^status: shipped" "$COMMENT_PLAN/00-INDEX.md" \
    && pass "REGRESSION: status written despite comment lines" \
    || fail "REGRESSION: status missing after stamp on commented plan"

# ---------------------------------------------------------------------------
# Block-list parsing (Bug 2 from 2026-04-28-target-state-plumbing-fixes plan)
# Writer emits inline-list `urls: [a, b]`, but external formatters normalize
# to block-list form `urls:\n  - a\n  - b`. The parser must accept both so
# graduate (and any sibling round-trip verb) keeps working after a formatter
# touches a stamped plan.
# ---------------------------------------------------------------------------
echo ""
echo "=== Block-list parsing ==="

echo "--- BL-AC1-HP: inline list round-trip (regression guard) ---"
BL_INLINE=$(make_folder_plan "plan-bl-inline")
"${STAMP_PY[@]}" stamp \
    --plan-path "$BL_INLINE" \
    --session-id BL_SID1 \
    --url https://github.com/org/repo/pull/1 >/dev/null
"${STAMP_PY[@]}" graduate --plan-path "$BL_INLINE"
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "BL-AC1-HP: graduate after inline-stamp exits 0" \
    || fail "BL-AC1-HP: graduate after inline-stamp exited $EXIT"
STATUS=$(frontmatter_value "$BL_INLINE/00-INDEX.md" "status")
[[ "$STATUS" == "done" ]] && pass "BL-AC1-HP: status flipped to done" \
    || fail "BL-AC1-HP: status expected 'done', got '$STATUS'"

echo "--- BL-AC2-HP: block-list (formatter-normalized) parses + graduates ---"
BL_BLOCK="$TMP/plan-bl-block"
mkdir -p "$BL_BLOCK"
cat > "$BL_BLOCK/00-INDEX.md" <<'EOF'
---
created: 2026-04-28T10:00:00Z
scope: feature
status: shipped
shipped_at: 2026-04-28T10:00:00Z
urls:
  - https://github.com/org/repo/pull/1
session_ids:
  - SID_X
expected_url_count: 1
---

# Block-list plan
EOF
GRAD_OUT=$("${STAMP_PY[@]}" graduate --plan-path "$BL_BLOCK" 2>&1)
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "BL-AC2-HP: graduate on block-list frontmatter exits 0" \
    || fail "BL-AC2-HP: graduate failed (exit=$EXIT, output: $GRAD_OUT)"
STATUS=$(frontmatter_value "$BL_BLOCK/00-INDEX.md" "status")
[[ "$STATUS" == "done" ]] && pass "BL-AC2-HP: block-list plan graduated to done" \
    || fail "BL-AC2-HP: status expected 'done', got '$STATUS'"

echo "--- BL-AC3-EDGE: empty block-list parses as empty list, not None ---"
BL_EMPTY="$TMP/plan-bl-empty"
mkdir -p "$BL_EMPTY"
cat > "$BL_EMPTY/00-INDEX.md" <<'EOF'
---
created: 2026-04-28T10:00:00Z
scope: feature
status: shipped
urls:
session_ids:
expected_url_count: 1
---

# Empty block-list
EOF
# Stamp adds a URL; without empty-block-list -> [] handling the parser would
# either crash or coerce the empty value to a string scalar, breaking the
# accumulate-URLs path in cmd_stamp.
STAMP_OUT=$("${STAMP_PY[@]}" stamp \
    --plan-path "$BL_EMPTY" \
    --session-id BL_EMPTY_SID \
    --url https://github.com/org/repo/pull/2 2>&1)
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "BL-AC3-EDGE: stamp on empty block-list exits 0" \
    || fail "BL-AC3-EDGE: stamp failed (exit=$EXIT, output: $STAMP_OUT)"
URLS=$(frontmatter_list "$BL_EMPTY/00-INDEX.md" "urls" 2>/dev/null)
[[ "$URLS" == *"pull/2"* ]] && pass "BL-AC3-EDGE: new URL appended to empty list" \
    || fail "BL-AC3-EDGE: urls missing pull/2 (got '$URLS')"

echo "--- BL-AC5-EDGE: blank + comment lines inside block-list ---"
# The parser explicitly handles blanks and `#`-comments inside a block-list.
# Without a fixture, a future refactor could break this silently.
BL_BLANKS="$TMP/plan-bl-blanks"
mkdir -p "$BL_BLANKS"
cat > "$BL_BLANKS/00-INDEX.md" <<'EOF'
---
created: 2026-04-28T10:00:00Z
scope: feature
status: shipped
urls:
  - https://github.com/org/repo/pull/1

  # follow-up reference
  - https://github.com/org/repo/pull/2
session_ids:
  - SID_X
expected_url_count: 2
---

# Block-list with blanks + comments
EOF
GRAD_OUT=$("${STAMP_PY[@]}" graduate --plan-path "$BL_BLANKS" 2>&1)
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "BL-AC5-EDGE: graduate parses block-list with blanks + comments" \
    || fail "BL-AC5-EDGE: graduate failed (exit=$EXIT, output: $GRAD_OUT)"
URLS=$(frontmatter_list "$BL_BLANKS/00-INDEX.md" "urls" 2>/dev/null)
[[ "$URLS" == *"pull/1"* && "$URLS" == *"pull/2"* ]] \
    && pass "BL-AC5-EDGE: both URLs preserved across blank + comment lines" \
    || fail "BL-AC5-EDGE: lost URLs across blanks/comments (got '$URLS')"
STATUS=$(frontmatter_value "$BL_BLANKS/00-INDEX.md" "status")
[[ "$STATUS" == "done" ]] && pass "BL-AC5-EDGE: status flipped (count=2 met expected_url_count=2)" \
    || fail "BL-AC5-EDGE: status expected 'done', got '$STATUS'"

echo "--- BL-AC6-HP: stamp accumulates onto a populated block-list ---"
# Bug-shape test: a stamped plan whose external-formatter normalized urls
# into block form must accept a SECOND ship's URL without dropping the first.
# Simulates the literal "format-after-stamp -> stamp again" sequence.
BL_ACCUM="$TMP/plan-bl-accum"
mkdir -p "$BL_ACCUM"
cat > "$BL_ACCUM/00-INDEX.md" <<'EOF'
---
created: 2026-04-28T10:00:00Z
scope: feature
status: shipped
shipped_at: 2026-04-28T10:00:00Z
urls:
  - https://github.com/org/repo/pull/1
session_ids:
  - SID_FIRST
expected_url_count: 2
---

# Populated block-list ready for accumulation
EOF
"${STAMP_PY[@]}" stamp \
    --plan-path "$BL_ACCUM" \
    --session-id SID_SECOND \
    --url https://github.com/org/repo/pull/2 >/dev/null
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "BL-AC6-HP: second stamp on block-list exits 0" \
    || fail "BL-AC6-HP: second stamp failed (exit=$EXIT)"
URLS=$(frontmatter_list "$BL_ACCUM/00-INDEX.md" "urls" 2>/dev/null)
[[ "$URLS" == *"pull/1"* && "$URLS" == *"pull/2"* ]] \
    && pass "BL-AC6-HP: original URL preserved AND new URL appended" \
    || fail "BL-AC6-HP: accumulate clobbered URLs (got '$URLS')"
SIDS=$(frontmatter_list "$BL_ACCUM/00-INDEX.md" "session_ids" 2>/dev/null)
[[ "$SIDS" == *"SID_FIRST"* && "$SIDS" == *"SID_SECOND"* ]] \
    && pass "BL-AC6-HP: original session_id preserved AND new appended" \
    || fail "BL-AC6-HP: session_ids accumulate broken (got '$SIDS')"

echo "--- BL-AC4-ERR: genuinely-nested mapping still raises ValueError ---"
# Indented `key: value` (NOT a `- ` list item) under a bare-key parent must
# still fail. This is the existing 1.2-AC5-ERR contract but explicit at the
# parser-edge level so the new branch doesn't silently start accepting maps.
BL_NESTED="$TMP/plan-bl-nested"
mkdir -p "$BL_NESTED"
cat > "$BL_NESTED/00-INDEX.md" <<'EOF'
---
created: 2026-04-28T10:00:00Z
scope: feature
config:
  flag: true
---

# Nested mapping
EOF
ERR_OUT=$("${STAMP_PY[@]}" stamp --plan-path "$BL_NESTED" \
    --session-id BL_NESTED_SID --url URL_X 2>&1)
EXIT=$?
[[ $EXIT -ne 0 ]] && pass "BL-AC4-ERR: nested mapping exits non-zero" \
    || fail "BL-AC4-ERR: expected non-zero exit on nested mapping"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"
exit "$FAIL"
