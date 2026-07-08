#!/usr/bin/env bash
# test-ship-stamp-integration.sh -- end-to-end integration tests for the plan
# completion stamp pipeline.
#
# Covers five scenarios:
#   1. Single-project folder plan: stamp once, graduate, confirm status: done
#   2. Single-project quick plan (single file): stamp, confirm no COMPLETION.md,
#      no .completed/, graduate, confirm done
#   3. Cross-project folder plan (expected_url_count: 2): stamp twice across two
#      sessions, graduate after each, confirm shipped then done
#   4. Idempotent re-stamp: identical (session, url) produces no duplicates
#   5. Backfill path: simulate stop-hook backfill via direct stamp-plan.py call
#      and assert hook contains the backfill logic (lite variant - see comment)
#
# Usage: bash tests/test-ship-stamp-integration.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# The stamper is now an in-package module run via `python3 -m fno.plan._stamp`.
# Put cli/src on PYTHONPATH so the module resolves when running from the repo.
export PYTHONPATH="$REPO_ROOT/cli/src${PYTHONPATH:+:$PYTHONPATH}"
STAMP_PY=(python3 -m fno.plan._stamp)

TMP=$(mktemp -d -t stamp-integration.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
SCENARIO_PASS=0
SCENARIO_FAIL=0

pass() { echo "    PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "    FAIL: $1"; FAIL=$((FAIL + 1)); }

scenario_start() {
    echo ""
    echo "=== Scenario $1: $2 ==="
}

scenario_end() {
    local snum="$1"
    local prev_fail="$2"
    if [[ "$FAIL" -eq "$prev_fail" ]]; then
        echo "  SCENARIO $snum: OK"
        SCENARIO_PASS=$((SCENARIO_PASS + 1))
    else
        echo "  SCENARIO $snum: FAIL ($(( FAIL - prev_fail )) assertion(s) failed)"
        SCENARIO_FAIL=$((SCENARIO_FAIL + 1))
    fi
}

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

make_folder_plan() {
    # $1 = subdir name under $TMP
    local dir="$TMP/$1"
    mkdir -p "$dir"
    cat > "$dir/00-INDEX.md" <<'EOF'
---
created: 2026-04-21T10:00:00Z
scope: feature
---

# Integration Test Plan
EOF
    echo "$dir"
}

make_quick_plan() {
    # $1 = filename under $TMP (e.g. quick.md)
    local fpath="$TMP/$1"
    cat > "$fpath" <<'EOF'
---
created: 2026-04-21T10:00:00Z
scope: quick-scope
---

# Quick Integration Test Plan
EOF
    echo "$fpath"
}

frontmatter_value() {
    # Extract a scalar value from frontmatter: frontmatter_value <file> <key>
    python3 - "$1" "$2" <<'PYEOF'
import sys, re
path, key = sys.argv[1], sys.argv[2]
text = open(path).read()
m = re.match(r'^---\n(.*?)\n---', text, re.DOTALL)
if not m:
    sys.exit(0)
fm = m.group(1)
pat = re.compile(r'^' + re.escape(key) + r':\s*(.+)$', re.MULTILINE)
hit = pat.search(fm)
if hit:
    print(hit.group(1).strip())
PYEOF
}

frontmatter_list_count() {
    # Count list items for a key: frontmatter_list_count <file> <key>
    python3 - "$1" "$2" <<'PYEOF'
import sys, re
path, key = sys.argv[1], sys.argv[2]
text = open(path).read()
m = re.match(r'^---\n(.*?)\n---', text, re.DOTALL)
if not m:
    print(0)
    sys.exit(0)
fm = m.group(1)
# Find the block under the key, count "- " prefixed lines
in_block = False
count = 0
for line in fm.splitlines():
    if re.match(r'^' + re.escape(key) + r':', line):
        in_block = True
        # Inline list: key: [a, b] or key: [a]
        inline = re.search(r'\[(.+)\]', line)
        if inline:
            items = [x.strip() for x in inline.group(1).split(',') if x.strip()]
            print(len(items))
            sys.exit(0)
        continue
    if in_block:
        if line.startswith('  - ') or line.startswith('- '):
            count += 1
        elif line and not line.startswith(' '):
            break
print(count)
PYEOF
}

frontmatter_contains() {
    # Check if frontmatter contains a string: frontmatter_contains <file> <string>
    python3 - "$1" "$2" <<'PYEOF'
import sys, re
path, needle = sys.argv[1], sys.argv[2]
text = open(path).read()
m = re.match(r'^---\n(.*?)\n---', text, re.DOTALL)
if not m:
    sys.exit(1)
sys.exit(0 if needle in m.group(1) else 1)
PYEOF
}

# ---------------------------------------------------------------------------
# Scenario 1: Single-project folder plan
# ---------------------------------------------------------------------------
scenario_start 1 "Single-project folder plan: stamp once, graduate, confirm done"
S1_FAIL_BEFORE="$FAIL"

S1_DIR=$(make_folder_plan "s1-folder")
S1_INDEX="$S1_DIR/00-INDEX.md"

"${STAMP_PY[@]}" stamp \
    --plan-path "$S1_DIR" \
    --session-id "SID1" \
    --url "http://example.com/pull/1" \
    --completion-note "Scenario 1 completion" \
    2>/dev/null

STATUS=$(frontmatter_value "$S1_INDEX" "status")
[[ "$STATUS" == "shipped" ]] && pass "s1: status is 'shipped' after stamp" \
    || fail "s1: expected status=shipped, got '$STATUS'"

URL_COUNT=$(frontmatter_list_count "$S1_INDEX" "urls")
[[ "$URL_COUNT" -eq 1 ]] && pass "s1: urls has 1 entry" \
    || fail "s1: expected 1 url, got $URL_COUNT"

frontmatter_contains "$S1_INDEX" "SID1" && pass "s1: session_ids contains SID1" \
    || fail "s1: session_ids missing SID1"

SHIPPED_AT=$(frontmatter_value "$S1_INDEX" "shipped_at")
[[ -n "$SHIPPED_AT" ]] && pass "s1: shipped_at is present" \
    || fail "s1: shipped_at is missing"

[[ -f "$S1_DIR/COMPLETION.md" ]] && pass "s1: COMPLETION.md exists at folder root" \
    || fail "s1: COMPLETION.md missing"

"${STAMP_PY[@]}" graduate --plan-path "$S1_DIR" 2>/dev/null

STATUS=$(frontmatter_value "$S1_INDEX" "status")
[[ "$STATUS" == "done" ]] && pass "s1: status is 'done' after graduate" \
    || fail "s1: expected status=done after graduate, got '$STATUS'"

# Other fields must survive graduation
frontmatter_contains "$S1_INDEX" "SID1" && pass "s1: session_ids unchanged after graduate" \
    || fail "s1: session_ids lost after graduate"
SHIPPED_AT2=$(frontmatter_value "$S1_INDEX" "shipped_at")
[[ "$SHIPPED_AT2" == "$SHIPPED_AT" ]] && pass "s1: shipped_at unchanged after graduate" \
    || fail "s1: shipped_at changed after graduate"

scenario_end 1 "$S1_FAIL_BEFORE"

# ---------------------------------------------------------------------------
# Scenario 2: Single-project quick plan (single file)
# ---------------------------------------------------------------------------
scenario_start 2 "Quick plan (single file): stamp, no COMPLETION.md, no .completed/, graduate"
S2_FAIL_BEFORE="$FAIL"

S2_FILE=$(make_quick_plan "s2-quick.md")

"${STAMP_PY[@]}" stamp \
    --plan-path "$S2_FILE" \
    --session-id "SID2" \
    --url "http://example.com/pull/2" \
    2>/dev/null

STATUS=$(frontmatter_value "$S2_FILE" "status")
[[ "$STATUS" == "shipped" ]] && pass "s2: quick plan frontmatter stamped (status=shipped)" \
    || fail "s2: expected status=shipped in quick plan, got '$STATUS'"

# No COMPLETION.md sibling for quick plans
COMPLETION_SIBLING="$(dirname "$S2_FILE")/COMPLETION.md"
[[ ! -f "$COMPLETION_SIBLING" ]] && pass "s2: no COMPLETION.md sibling" \
    || fail "s2: unexpected COMPLETION.md sibling for quick plan"

# No .completed/ anywhere in the temp dir
COMPLETED_DIRS=$(find "$TMP" -name ".completed" -type d 2>/dev/null | wc -l | tr -d ' ')
[[ "$COMPLETED_DIRS" -eq 0 ]] && pass "s2: no .completed/ directory created" \
    || fail "s2: .completed/ directory found (should not exist)"

"${STAMP_PY[@]}" graduate --plan-path "$S2_FILE" 2>/dev/null

STATUS=$(frontmatter_value "$S2_FILE" "status")
[[ "$STATUS" == "done" ]] && pass "s2: quick plan graduates to done" \
    || fail "s2: expected status=done after graduate, got '$STATUS'"

scenario_end 2 "$S2_FAIL_BEFORE"

# ---------------------------------------------------------------------------
# Scenario 3: Cross-project folder plan (expected_url_count: 2)
# ---------------------------------------------------------------------------
scenario_start 3 "Cross-project folder plan (2 expected URLs): two stamps, graduate each"
S3_FAIL_BEFORE="$FAIL"

S3_DIR=$(make_folder_plan "s3-cross")
S3_INDEX="$S3_DIR/00-INDEX.md"

# First stamp: one of two expected URLs
"${STAMP_PY[@]}" stamp \
    --plan-path "$S3_DIR" \
    --session-id "SID3A" \
    --url "http://example.com/repo1/pull/10" \
    --expected-url-count 2 \
    2>/dev/null

STATUS=$(frontmatter_value "$S3_INDEX" "status")
[[ "$STATUS" == "shipped" ]] && pass "s3: status=shipped after first stamp" \
    || fail "s3: expected status=shipped after first stamp, got '$STATUS'"

URL_COUNT=$(frontmatter_list_count "$S3_INDEX" "urls")
[[ "$URL_COUNT" -eq 1 ]] && pass "s3: urls has 1 entry after first stamp" \
    || fail "s3: expected 1 url after first stamp, got $URL_COUNT"

# Graduate after first stamp - should stay shipped (not enough URLs)
"${STAMP_PY[@]}" graduate --plan-path "$S3_DIR" 2>/dev/null

STATUS=$(frontmatter_value "$S3_INDEX" "status")
[[ "$STATUS" == "shipped" ]] && pass "s3: status remains 'shipped' after graduate with 1/2 URLs" \
    || fail "s3: expected status=shipped (not done) after first graduate, got '$STATUS'"

# Second stamp: second URL
"${STAMP_PY[@]}" stamp \
    --plan-path "$S3_DIR" \
    --session-id "SID3B" \
    --url "http://example.com/repo2/pull/11" \
    --expected-url-count 2 \
    2>/dev/null

URL_COUNT=$(frontmatter_list_count "$S3_INDEX" "urls")
[[ "$URL_COUNT" -eq 2 ]] && pass "s3: urls has 2 entries after second stamp" \
    || fail "s3: expected 2 urls after second stamp, got $URL_COUNT"

frontmatter_contains "$S3_INDEX" "SID3B" && pass "s3: session_ids contains SID3B" \
    || fail "s3: session_ids missing SID3B"

# Graduate after second stamp - now should be done
"${STAMP_PY[@]}" graduate --plan-path "$S3_DIR" 2>/dev/null

STATUS=$(frontmatter_value "$S3_INDEX" "status")
[[ "$STATUS" == "done" ]] && pass "s3: status=done after graduate with 2/2 URLs" \
    || fail "s3: expected status=done after second graduate, got '$STATUS'"

scenario_end 3 "$S3_FAIL_BEFORE"

# ---------------------------------------------------------------------------
# Scenario 4: Idempotent re-stamp
# ---------------------------------------------------------------------------
scenario_start 4 "Idempotent re-stamp: identical (session, url) produces no duplicates"
S4_FAIL_BEFORE="$FAIL"

S4_DIR=$(make_folder_plan "s4-idem")
S4_INDEX="$S4_DIR/00-INDEX.md"

"${STAMP_PY[@]}" stamp \
    --plan-path "$S4_DIR" \
    --session-id "SID4" \
    --url "http://example.com/pull/4" \
    2>/dev/null

SHIPPED_AT_FIRST=$(frontmatter_value "$S4_INDEX" "shipped_at")

# Stamp again with the same session + url
"${STAMP_PY[@]}" stamp \
    --plan-path "$S4_DIR" \
    --session-id "SID4" \
    --url "http://example.com/pull/4" \
    2>/dev/null
EXIT_CODE=$?

[[ "$EXIT_CODE" -eq 0 ]] && pass "s4: re-stamp exits 0" \
    || fail "s4: re-stamp exited non-zero ($EXIT_CODE)"

URL_COUNT=$(frontmatter_list_count "$S4_INDEX" "urls")
[[ "$URL_COUNT" -eq 1 ]] && pass "s4: no duplicate URL entries" \
    || fail "s4: expected 1 url entry, got $URL_COUNT (idempotency broken)"

SID_COUNT=$(python3 - "$S4_INDEX" "SID4" <<'PYEOF'
import sys, re
path, sid = sys.argv[1], sys.argv[2]
text = open(path).read()
m = re.match(r'^---\n(.*?)\n---', text, re.DOTALL)
fm = m.group(1) if m else ""
print(fm.count(sid))
PYEOF
)
[[ "$SID_COUNT" -eq 1 ]] && pass "s4: no duplicate session_id entries" \
    || fail "s4: expected 1 session_id entry, got $SID_COUNT (idempotency broken)"

SHIPPED_AT_SECOND=$(frontmatter_value "$S4_INDEX" "shipped_at")
[[ "$SHIPPED_AT_SECOND" == "$SHIPPED_AT_FIRST" ]] && pass "s4: shipped_at unchanged after re-stamp" \
    || fail "s4: shipped_at was overwritten (should be immutable)"

scenario_end 4 "$S4_FAIL_BEFORE"

# ---------------------------------------------------------------------------
# Scenario 5: Backfill path (lite variant)
# ---------------------------------------------------------------------------
# The stop hook's backfill block calls stamp-plan.py directly when:
#   - .fno/artifacts/ship-{session_id}.md exists and is non-empty
#   - plan_path is set in target-state.md
#   - the session_id is NOT already in the plan's frontmatter
#   - pr_url is set in target-state.md
#
# Invoking the full stop hook requires a complete Claude session fixture and
# triggers side effects (cost calculation, artifact archival) that are hard to
# isolate without a real git repo and session files.
#
# Lite approach:
#   1. Set up the fixture files (target-state.md + ship artifact + unstamped plan)
#   2. Call stamp-plan.py directly (replicating what the hook does)
#   3. Assert the plan is stamped correctly
#   4. Assert via grep that hooks/target-stop-hook.sh contains the backfill logic
#      (structural check that the wiring exists in the hook)
#
scenario_start 5 "Backfill path (lite): fixture setup + direct stamp, hook wiring verified via grep"
S5_FAIL_BEFORE="$FAIL"

S5_REPO="$TMP/s5-repo"
mkdir -p "$S5_REPO/.fno/artifacts"
mkdir -p "$S5_REPO/plans/my-plan"

# Unstamped folder plan
cat > "$S5_REPO/plans/my-plan/00-INDEX.md" <<'EOF'
---
created: 2026-04-21T10:00:00Z
scope: feature
---

# Backfill Test Plan
EOF

S5_SESSION="20260422T120000Z-99999-abc123"
S5_PR_URL="http://example.com/pull/99"

# target-state.md fixture (mimics what the stop hook reads)
cat > "$S5_REPO/.fno/target-state.md" <<EOF
---
status: COMPLETE
input_type: plan
plan_path: plans/my-plan
pr_url: $S5_PR_URL
session_id: $S5_SESSION
iteration: 1
---
EOF

# ship artifact (non-empty, mimics .fno/artifacts/ship-{session_id}.md)
cat > "$S5_REPO/.fno/artifacts/ship-${S5_SESSION}.md" <<'EOF'
## Ship Gate Attestation

PR created and merged. Backfill test fixture.
EOF

# The hook reads plan_path, resolves it relative to REPO_ROOT, and calls:
#   python3 stamp-plan.py stamp --plan-path <abs_plan> --session-id <sid> --url <pr_url>
# We replicate that exact invocation here.
ABS_PLAN="$S5_REPO/plans/my-plan"
"${STAMP_PY[@]}" stamp \
    --plan-path "$ABS_PLAN" \
    --session-id "$S5_SESSION" \
    --url "$S5_PR_URL" \
    --completion-note "$(cat "$S5_REPO/.fno/artifacts/ship-${S5_SESSION}.md")" \
    2>/dev/null

S5_INDEX="$ABS_PLAN/00-INDEX.md"

STATUS=$(frontmatter_value "$S5_INDEX" "status")
[[ "$STATUS" == "shipped" ]] && pass "s5: backfill stamps plan with status=shipped" \
    || fail "s5: expected status=shipped after backfill, got '$STATUS'"

frontmatter_contains "$S5_INDEX" "$S5_SESSION" && pass "s5: backfill includes session_id" \
    || fail "s5: backfill missing session_id"

frontmatter_contains "$S5_INDEX" "$S5_PR_URL" && pass "s5: backfill includes pr_url" \
    || fail "s5: backfill missing pr_url"

# Structural check: assert the stop hook contains the backfill wiring
HOOK_FILE="$REPO_ROOT/hooks/target-stop-hook.sh"
grep -q "Backfill plan stamp" "$HOOK_FILE" 2>/dev/null \
    && pass "s5: hook contains 'Backfill plan stamp' log line (wiring present)" \
    || fail "s5: hook missing backfill wiring (grep for 'Backfill plan stamp' failed)"

grep -q 'stamp-plan.py\|stamp_script.*stamp\|"$stamp_script" stamp' "$HOOK_FILE" 2>/dev/null \
    && pass "s5: hook invokes stamp-plan.py (call site present)" \
    || fail "s5: hook missing stamp-plan.py invocation"

grep -q "ship_artifact\|ship-\${session_id}" "$HOOK_FILE" 2>/dev/null \
    && pass "s5: hook references ship artifact path (trigger condition present)" \
    || fail "s5: hook missing ship artifact path reference"

scenario_end 5 "$S5_FAIL_BEFORE"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "================================"
echo "Scenarios: $SCENARIO_PASS passed, $SCENARIO_FAIL failed"
echo "Assertions: $PASS passed, $FAIL failed"
echo "================================"

[[ "$SCENARIO_FAIL" -eq 0 ]]
