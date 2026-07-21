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

make_plan() {
    # $1 = file name under $TMP (e.g. plan-ac1hp.md); $2 optional extra frontmatter lines
    local fpath="$TMP/$1"
    local extra="${2:-}"
    cat > "$fpath" <<EOF
---
created: 2026-04-21T10:00:00Z
scope: test-scope
${extra}---

# Test Plan
EOF
    echo "$fpath"
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
# Task 1.1: Stamp a plan with no prior stamp (AC1-HP)
# ---------------------------------------------------------------------------
echo ""
echo "=== Task 1.1 ==="

echo "--- AC1-HP: Stamp plan with no prior stamp ---"
PLAN_FILE=$(make_plan "plan-ac1hp.md")
"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_FILE" \
    --session-id SID1 \
    --url https://github.com/org/repo/pull/1
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "AC1-HP: exit 0" || fail "AC1-HP: expected exit 0, got $EXIT"

STATUS=$(frontmatter_value "$PLAN_FILE" "status" 2>/dev/null)
[[ "$STATUS" == "in_review" ]] && pass "AC1-HP: status=in_review" || fail "AC1-HP: status expected 'in_review', got '$STATUS'"

SHIPPED_AT=$(frontmatter_value "$PLAN_FILE" "shipped_at" 2>/dev/null)
[[ -n "$SHIPPED_AT" ]] && pass "AC1-HP: shipped_at present" || fail "AC1-HP: shipped_at missing"

URLS=$(frontmatter_list "$PLAN_FILE" "urls" 2>/dev/null)
[[ "$URLS" == *"https://github.com/org/repo/pull/1"* ]] && pass "AC1-HP: urls contains URL1" || fail "AC1-HP: urls missing URL1 (got '$URLS')"

SIDS=$(frontmatter_list "$PLAN_FILE" "session_ids" 2>/dev/null)
[[ "$SIDS" == *"SID1"* ]] && pass "AC1-HP: session_ids contains SID1" || fail "AC1-HP: session_ids missing SID1 (got '$SIDS')"

# Original fields preserved byte-for-byte
CREATED=$(frontmatter_value "$PLAN_FILE" "created" 2>/dev/null)
[[ "$CREATED" == "2026-04-21T10:00:00Z" ]] && pass "AC1-HP: created field preserved" || fail "AC1-HP: created field corrupted (got '$CREATED')"

SCOPE=$(frontmatter_value "$PLAN_FILE" "scope" 2>/dev/null)
[[ "$SCOPE" == "test-scope" ]] && pass "AC1-HP: scope field preserved" || fail "AC1-HP: scope field corrupted (got '$SCOPE')"

# ---------------------------------------------------------------------------
# Task 1.1: Invalid plan path (AC2-ERR)
# ---------------------------------------------------------------------------
echo ""
echo "--- AC2-ERR: Invalid plan path ---"

# Non-existent path
"${STAMP_PY[@]}" stamp --plan-path "$TMP/no-such-path.md" --session-id SID1 --url URL1 2>/dev/null
EXIT=$?
[[ $EXIT -ne 0 ]] && pass "AC2-ERR: non-existent path exits non-zero" || fail "AC2-ERR: non-existent path should exit non-zero"

# A directory (not a file) as --plan-path: folder plans are no longer
# supported, so this must fail even though the path itself exists.
NOT_A_FILE="$TMP/a-directory"
mkdir -p "$NOT_A_FILE"
"${STAMP_PY[@]}" stamp --plan-path "$NOT_A_FILE" --session-id SID1 --url URL1 2>/dev/null
EXIT=$?
[[ $EXIT -ne 0 ]] && pass "AC2-ERR: directory as plan-path exits non-zero" || fail "AC2-ERR: directory as plan-path should exit non-zero"

# Stderr message for missing path
ERR_MSG=$("${STAMP_PY[@]}" stamp --plan-path "$TMP/missing.md" --session-id SID1 --url URL1 2>&1 >/dev/null)
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
[[ "$STATUS_Q" == "in_review" ]] && pass "AC4-EDGE: quick plan status=in_review" || fail "AC4-EDGE: quick plan status expected 'in_review', got '$STATUS_Q'"

# ---------------------------------------------------------------------------
# Task 1.2: Idempotent re-stamp (AC1-HP)
# ---------------------------------------------------------------------------
echo ""
echo "=== Task 1.2 ==="

echo "--- 1.2-AC1-HP: Idempotent re-stamp ---"
PLAN_IDEM=$(make_plan "plan-idem.md")
# First stamp
"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_IDEM" \
    --session-id SID1 \
    --url https://github.com/org/repo/pull/10
SHIPPED_AT_FIRST=$(frontmatter_value "$PLAN_IDEM" "shipped_at" 2>/dev/null)
CONTENT_BEFORE=$(cat "$PLAN_IDEM")

# Second stamp with same args
"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_IDEM" \
    --session-id SID1 \
    --url https://github.com/org/repo/pull/10
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "1.2-AC1-HP: re-stamp exits 0" || fail "1.2-AC1-HP: re-stamp should exit 0, got $EXIT"

CONTENT_AFTER=$(cat "$PLAN_IDEM")
[[ "$CONTENT_BEFORE" == "$CONTENT_AFTER" ]] && pass "1.2-AC1-HP: file unchanged on re-stamp" || fail "1.2-AC1-HP: file was modified on re-stamp"

SHIPPED_AT_SECOND=$(frontmatter_value "$PLAN_IDEM" "shipped_at" 2>/dev/null)
[[ "$SHIPPED_AT_FIRST" == "$SHIPPED_AT_SECOND" ]] && pass "1.2-AC1-HP: shipped_at not rewritten" || fail "1.2-AC1-HP: shipped_at changed on re-stamp"

URLS_IDEM=$(frontmatter_list "$PLAN_IDEM" "urls" 2>/dev/null)
# Count occurrences of the URL - should appear exactly once
URL_COUNT=$(echo "$URLS_IDEM" | grep -o "pull/10" | wc -l | tr -d ' ')
[[ "$URL_COUNT" -eq 1 ]] && pass "1.2-AC1-HP: no duplicate URL" || fail "1.2-AC1-HP: URL duplicated (count=$URL_COUNT)"

# ---------------------------------------------------------------------------
# Task 1.2: Accumulate across ships (AC2-HP)
# ---------------------------------------------------------------------------
echo ""
echo "--- 1.2-AC2-HP: Accumulate across ships ---"
PLAN_ACCUM=$(make_plan "plan-accum.md")

"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_ACCUM" \
    --session-id SID1 \
    --url https://github.com/org/repo/pull/20

FIRST_AT=$(frontmatter_value "$PLAN_ACCUM" "shipped_at" 2>/dev/null)

"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_ACCUM" \
    --session-id SID2 \
    --url https://github.com/org/repo2/pull/21 \
    --expected-url-count 2
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "1.2-AC2-HP: second stamp exits 0" || fail "1.2-AC2-HP: second stamp expected exit 0, got $EXIT"

URLS_ACCUM=$(frontmatter_list "$PLAN_ACCUM" "urls" 2>/dev/null)
[[ "$URLS_ACCUM" == *"pull/20"* ]] && pass "1.2-AC2-HP: URL1 preserved" || fail "1.2-AC2-HP: URL1 missing"
[[ "$URLS_ACCUM" == *"pull/21"* ]] && pass "1.2-AC2-HP: URL2 added" || fail "1.2-AC2-HP: URL2 missing"

SIDS_ACCUM=$(frontmatter_list "$PLAN_ACCUM" "session_ids" 2>/dev/null)
[[ "$SIDS_ACCUM" == *"SID1"* ]] && pass "1.2-AC2-HP: SID1 preserved" || fail "1.2-AC2-HP: SID1 missing"
[[ "$SIDS_ACCUM" == *"SID2"* ]] && pass "1.2-AC2-HP: SID2 added" || fail "1.2-AC2-HP: SID2 added"

STATUS_ACCUM=$(frontmatter_value "$PLAN_ACCUM" "status" 2>/dev/null)
[[ "$STATUS_ACCUM" == "in_review" ]] && pass "1.2-AC2-HP: status still in_review (not done)" || fail "1.2-AC2-HP: status expected 'in_review', got '$STATUS_ACCUM'"

SECOND_AT=$(frontmatter_value "$PLAN_ACCUM" "shipped_at" 2>/dev/null)
[[ "$FIRST_AT" == "$SECOND_AT" ]] && pass "1.2-AC2-HP: shipped_at unchanged on second stamp" || fail "1.2-AC2-HP: shipped_at changed on second stamp"

# ---------------------------------------------------------------------------
# Task 1.2: Graduate to done (AC3-HP)
# ---------------------------------------------------------------------------
echo ""
echo "--- 1.2-AC3-HP: Graduate to done ---"
PLAN_GRAD=$(make_plan "plan-grad.md")

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

STATUS_GRAD=$(frontmatter_value "$PLAN_GRAD" "status" 2>/dev/null)
[[ "$STATUS_GRAD" == "done" ]] && pass "1.2-AC3-HP: status=done after graduate" || fail "1.2-AC3-HP: status expected 'done', got '$STATUS_GRAD'"

SCOPE_GRAD=$(frontmatter_value "$PLAN_GRAD" "scope" 2>/dev/null)
[[ "$SCOPE_GRAD" == "test-scope" ]] && pass "1.2-AC3-HP: scope preserved after graduate" || fail "1.2-AC3-HP: scope corrupted after graduate"

# ---------------------------------------------------------------------------
# Task 1.2: Graduate when insufficient URLs (AC4-EDGE)
# ---------------------------------------------------------------------------
echo ""
echo "--- 1.2-AC4-EDGE: Graduate with insufficient URLs ---"
PLAN_INSUF=$(make_plan "plan-insuf.md")

"${STAMP_PY[@]}" stamp \
    --plan-path "$PLAN_INSUF" \
    --session-id SID1 \
    --url https://github.com/org/repo/pull/40 \
    --expected-url-count 2

"${STAMP_PY[@]}" graduate --plan-path "$PLAN_INSUF"
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "1.2-AC4-EDGE: graduate exits 0 when insufficient" || fail "1.2-AC4-EDGE: expected exit 0, got $EXIT"

STATUS_INSUF=$(frontmatter_value "$PLAN_INSUF" "status" 2>/dev/null)
[[ "$STATUS_INSUF" == "in_review" ]] && pass "1.2-AC4-EDGE: status stays in_review" || fail "1.2-AC4-EDGE: status expected 'in_review', got '$STATUS_INSUF'"

# ---------------------------------------------------------------------------
# Task 1.2: Malformed frontmatter (AC5-ERR)
# ---------------------------------------------------------------------------
echo ""
echo "--- 1.2-AC5-ERR: Malformed frontmatter ---"
MALFORMED="$TMP/malformed.md"
cat > "$MALFORMED" <<'EOF'
---
created: 2026-04-21T10:00:00Z
scope: test
  extra: value
---

# Malformed Plan
EOF

ERR_OUT=$("${STAMP_PY[@]}" stamp --plan-path "$MALFORMED" --session-id SID1 --url URL1 2>&1)
EXIT=$?
[[ $EXIT -ne 0 ]] && pass "1.2-AC5-ERR: malformed frontmatter exits non-zero" || fail "1.2-AC5-ERR: expected non-zero exit"
[[ -n "$ERR_OUT" ]] && pass "1.2-AC5-ERR: error message emitted" || fail "1.2-AC5-ERR: no error message"
CONTENT_AFTER=$(cat "$MALFORMED")
# File should not be modified - check it still has the offending indented line
[[ "$CONTENT_AFTER" == *"extra: value"* ]] && pass "1.2-AC5-ERR: file not modified on parse error" || fail "1.2-AC5-ERR: file was modified despite parse error"

# ---------------------------------------------------------------------------
# Review regression: parser must skip comment lines in frontmatter
# Gemini PR #159 review flagged that template frontmatter with commented
# examples (e.g., `# linear: {TEAM}-XXX`) would crash the parser.
# ---------------------------------------------------------------------------
echo ""
echo "--- REGRESSION: comment lines in frontmatter must not crash parser ---"
COMMENT_PLAN="$TMP/plan-comments.md"
cat > "$COMMENT_PLAN" <<EOF
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
    --url https://github.com/org/repo/pull/999
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "REGRESSION: comment-line frontmatter stamps cleanly" \
    || fail "REGRESSION: parser still crashes on comments (exit=$EXIT)"
grep -q "^status: in_review" "$COMMENT_PLAN" \
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
BL_INLINE=$(make_plan "plan-bl-inline.md")
"${STAMP_PY[@]}" stamp \
    --plan-path "$BL_INLINE" \
    --session-id BL_SID1 \
    --url https://github.com/org/repo/pull/1 >/dev/null
"${STAMP_PY[@]}" graduate --plan-path "$BL_INLINE"
EXIT=$?
[[ $EXIT -eq 0 ]] && pass "BL-AC1-HP: graduate after inline-stamp exits 0" \
    || fail "BL-AC1-HP: graduate after inline-stamp exited $EXIT"
STATUS=$(frontmatter_value "$BL_INLINE" "status")
[[ "$STATUS" == "done" ]] && pass "BL-AC1-HP: status flipped to done" \
    || fail "BL-AC1-HP: status expected 'done', got '$STATUS'"

echo "--- BL-AC2-HP: block-list (formatter-normalized) parses + graduates ---"
BL_BLOCK="$TMP/plan-bl-block.md"
cat > "$BL_BLOCK" <<'EOF'
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
STATUS=$(frontmatter_value "$BL_BLOCK" "status")
[[ "$STATUS" == "done" ]] && pass "BL-AC2-HP: block-list plan graduated to done" \
    || fail "BL-AC2-HP: status expected 'done', got '$STATUS'"

echo "--- BL-AC3-EDGE: empty block-list parses as empty list, not None ---"
BL_EMPTY="$TMP/plan-bl-empty.md"
cat > "$BL_EMPTY" <<'EOF'
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
URLS=$(frontmatter_list "$BL_EMPTY" "urls" 2>/dev/null)
[[ "$URLS" == *"pull/2"* ]] && pass "BL-AC3-EDGE: new URL appended to empty list" \
    || fail "BL-AC3-EDGE: urls missing pull/2 (got '$URLS')"

echo "--- BL-AC5-EDGE: blank + comment lines inside block-list ---"
# The parser explicitly handles blanks and `#`-comments inside a block-list.
# Without a fixture, a future refactor could break this silently.
BL_BLANKS="$TMP/plan-bl-blanks.md"
cat > "$BL_BLANKS" <<'EOF'
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
URLS=$(frontmatter_list "$BL_BLANKS" "urls" 2>/dev/null)
[[ "$URLS" == *"pull/1"* && "$URLS" == *"pull/2"* ]] \
    && pass "BL-AC5-EDGE: both URLs preserved across blank + comment lines" \
    || fail "BL-AC5-EDGE: lost URLs across blanks/comments (got '$URLS')"
STATUS=$(frontmatter_value "$BL_BLANKS" "status")
[[ "$STATUS" == "done" ]] && pass "BL-AC5-EDGE: status flipped (count=2 met expected_url_count=2)" \
    || fail "BL-AC5-EDGE: status expected 'done', got '$STATUS'"

echo "--- BL-AC6-HP: stamp accumulates onto a populated block-list ---"
# Bug-shape test: a stamped plan whose external-formatter normalized urls
# into block form must accept a SECOND ship's URL without dropping the first.
# Simulates the literal "format-after-stamp -> stamp again" sequence.
BL_ACCUM="$TMP/plan-bl-accum.md"
cat > "$BL_ACCUM" <<'EOF'
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
URLS=$(frontmatter_list "$BL_ACCUM" "urls" 2>/dev/null)
[[ "$URLS" == *"pull/1"* && "$URLS" == *"pull/2"* ]] \
    && pass "BL-AC6-HP: original URL preserved AND new URL appended" \
    || fail "BL-AC6-HP: accumulate clobbered URLs (got '$URLS')"
SIDS=$(frontmatter_list "$BL_ACCUM" "session_ids" 2>/dev/null)
[[ "$SIDS" == *"SID_FIRST"* && "$SIDS" == *"SID_SECOND"* ]] \
    && pass "BL-AC6-HP: original session_id preserved AND new appended" \
    || fail "BL-AC6-HP: session_ids accumulate broken (got '$SIDS')"

echo "--- BL-AC4-ERR: stray indented continuation after a scalar still raises ValueError ---"
# An indented line following a scalar-valued key (not a bare key opening a
# block) is genuinely unparseable and must still fail. A bare key followed by
# an indented mapping (e.g. `config:\n  flag: true`) is NOT this case - the
# parser now preserves that opaquely as a RawBlock (kill_criteria-style
# pass-through), so it is intentionally excluded from this fixture.
BL_NESTED="$TMP/plan-bl-nested.md"
cat > "$BL_NESTED" <<'EOF'
---
created: 2026-04-28T10:00:00Z
scope: feature
  flag: true
---

# Stray indented continuation
EOF
ERR_OUT=$("${STAMP_PY[@]}" stamp --plan-path "$BL_NESTED" \
    --session-id BL_NESTED_SID --url URL_X 2>&1)
EXIT=$?
[[ $EXIT -ne 0 ]] && pass "BL-AC4-ERR: stray indented continuation exits non-zero" \
    || fail "BL-AC4-ERR: expected non-zero exit on stray indented continuation"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"
exit "$FAIL"
