#!/usr/bin/env bash
# Tests for scripts/memory/post-merge-pass.sh (Task 1.2).
# Stubs `gh` via PATH override; exercises sentinel lifecycle and JSON output.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PASS_SCRIPT="$REPO_ROOT/scripts/memory/post-merge-pass.sh"

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; FAILURES=$((FAILURES + 1)); }

FAILURES=0

[[ -x "$PASS_SCRIPT" ]] || { echo "FAIL: post-merge-pass.sh not executable at $PASS_SCRIPT" >&2; exit 1; }

# -------------------------------------------------------------------
# Shared fixtures
# -------------------------------------------------------------------

make_stub_gh() {
    local stub_dir="$1"
    local merged_at="${2:-2026-05-05T01:00:00Z}"
    local late_comment_ts="${3:-2026-05-05T02:00:00Z}"
    local pr_state="${4:-MERGED}"
    # Build the mergedAt JSON value (literal null OR JSON-quoted string).
    local merged_at_json
    if [[ "$merged_at" == "null" ]]; then
        merged_at_json="null"
    else
        merged_at_json="\"$merged_at\""
    fi
    mkdir -p "$stub_dir"
    cat > "$stub_dir/gh" <<STUB
#!/usr/bin/env bash
# Minimal gh stub for post-merge-pass tests.
# Dispatches on arg patterns.
case "\$*" in
    *"pr view"*"--json state,mergedAt"*|*"pr view"*"--json mergedAt"*)
        printf '%s' '{"state":"$pr_state","mergedAt":$merged_at_json}'
        ;;
    *"repos/"*"/issues/"*"/comments"*)
        printf '[{"user":{"login":"reviewer"},"body":"post-merge comment","created_at":"%s"}]' "$late_comment_ts"
        ;;
    *"repos/"*"/pulls/"*"/reviews"*)
        printf '[{"user":{"login":"reviewer"},"body":"looks good after merge","state":"APPROVED","submitted_at":"%s"}]' "$late_comment_ts"
        ;;
    *"repo view"*"--json owner"*)
        echo '"testowner"'
        ;;
    *"repo view"*"--json name"*)
        echo '"testrepo"'
        ;;
    *)
        echo "stub gh: unmatched args: \$*" >&2
        exit 1
        ;;
esac
STUB
    chmod +x "$stub_dir/gh"
}

make_stub_gh_no_late_signal() {
    local stub_dir="$1"
    local merged_at="${2:-2026-05-05T01:00:00Z}"
    local pr_state="${3:-MERGED}"
    mkdir -p "$stub_dir"
    cat > "$stub_dir/gh" <<STUB
#!/usr/bin/env bash
case "\$*" in
    *"pr view"*"--json state,mergedAt"*|*"pr view"*"--json mergedAt"*)
        printf '{"state":"%s","mergedAt":"%s"}' "$pr_state" "$merged_at"
        ;;
    *"repos/"*"/issues/"*"/comments"*)
        echo '[]'
        ;;
    *"repos/"*"/pulls/"*"/reviews"*)
        echo '[]'
        ;;
    *"repo view"*"--json owner"*)
        echo '"testowner"'
        ;;
    *"repo view"*"--json name"*)
        echo '"testrepo"'
        ;;
    *)
        echo "stub gh: unmatched args: \$*" >&2
        exit 1
        ;;
esac
STUB
    chmod +x "$stub_dir/gh"
}

# -------------------------------------------------------------------
# AC1.2-HP: Sentinel absent -> exit 0 silently, no output
# -------------------------------------------------------------------
test_no_sentinel_exits_silently() {
    local work
    work=$(mktemp -d -t pmp-test-XXXXXX)
    mkdir -p "$work/.fno"
    # No sentinel file present.
    local stub_dir
    stub_dir=$(mktemp -d -t pmp-stub-XXXXXX)
    make_stub_gh "$stub_dir"

    local out
    out=$(GIT_DIR="$work/.git" \
          HOME="$work" \
          PATH="$stub_dir:$PATH" \
          bash "$PASS_SCRIPT" 2>/dev/null)
    local rc=$?

    [[ "$rc" -eq 0 ]] || fail "AC1.2-HP no-sentinel: expected exit 0, got $rc"
    [[ -z "$out" ]] || fail "AC1.2-HP no-sentinel: expected empty output, got: $out"
    pass "AC1.2-HP: absent sentinel -> exit 0 silently"

    rm -rf "$work" "$stub_dir"
}

# -------------------------------------------------------------------
# AC1.2-EDGE: Sentinel content empty -> exit 0, sentinel removed
# -------------------------------------------------------------------
test_empty_sentinel_cleaned_up() {
    local work
    work=$(mktemp -d -t pmp-test-XXXXXX)
    mkdir -p "$work/.fno"
    # Write empty sentinel.
    touch "$work/.fno/.memory-pass-pending"

    local stub_dir
    stub_dir=$(mktemp -d -t pmp-stub-XXXXXX)
    make_stub_gh "$stub_dir"

    local rc
    # Run from a temp dir that has git root = work via GIT_DIR trick.
    # The script calls `git rev-parse --show-toplevel` so we override by
    # running it with REPO_ROOT patched via a wrapper.
    (
        cd "$work"
        git init -q 2>/dev/null || true
        PATH="$stub_dir:$PATH" bash "$PASS_SCRIPT" 2>/dev/null
    )
    rc=$?

    [[ "$rc" -eq 0 ]] || fail "AC1.2-EDGE empty-sentinel: expected exit 0, got $rc"
    [[ ! -f "$work/.fno/.memory-pass-pending" ]] \
        || fail "AC1.2-EDGE empty-sentinel: sentinel should be removed after empty-content run"
    pass "AC1.2-EDGE: empty sentinel content -> exit 0, sentinel cleaned up"

    rm -rf "$work" "$stub_dir"
}

# -------------------------------------------------------------------
# AC1.2-HP: Late comment captured in JSON output
# -------------------------------------------------------------------
test_late_comment_in_json() {
    local work
    work=$(mktemp -d -t pmp-test-XXXXXX)
    mkdir -p "$work/.fno"
    echo "42" > "$work/.fno/.memory-pass-pending"

    local stub_dir
    stub_dir=$(mktemp -d -t pmp-stub-XXXXXX)
    # late comment at 02:00, merged at 01:00 -> late
    make_stub_gh "$stub_dir" "2026-05-05T01:00:00Z" "2026-05-05T02:00:00Z"

    local out rc
    (
        cd "$work"
        git init -q 2>/dev/null || true
        PATH="$stub_dir:$PATH" bash "$PASS_SCRIPT"
    ) > /tmp/pmp_test_out.json 2>/tmp/pmp_test_err.txt
    rc=$?

    out=$(cat /tmp/pmp_test_out.json)

    [[ "$rc" -eq 0 ]] || fail "AC1.2-HP late-comment: expected exit 0, got $rc (stderr: $(cat /tmp/pmp_test_err.txt))"

    # Validate JSON has required keys
    echo "$out" | python3 -c "import json,sys; d=json.load(sys.stdin); assert 'pr' in d, 'missing pr'; assert 'merged_at' in d, 'missing merged_at'; assert 'late_comments' in d, 'missing late_comments'; assert 'late_reviews' in d, 'missing late_reviews'; assert 'done_with_concerns' in d, 'missing done_with_concerns'" \
        || fail "AC1.2-HP late-comment: JSON missing required keys. Output: $out"

    echo "$out" | python3 -c "import json,sys; d=json.load(sys.stdin); assert len(d['late_comments']) >= 1, 'expected at least 1 late comment, got: ' + str(d['late_comments'])" \
        || fail "AC1.2-HP late-comment: late_comments should have at least 1 entry. Output: $out"

    pass "AC1.2-HP: late comment captured in JSON output"

    # Sentinel should be removed
    [[ ! -f "$work/.fno/.memory-pass-pending" ]] \
        || fail "AC1.2-FR sentinel-one-shot: sentinel should be removed after success"
    pass "AC1.2-FR: sentinel removed after successful run"

    rm -rf "$work" "$stub_dir" /tmp/pmp_test_out.json /tmp/pmp_test_err.txt
}

# -------------------------------------------------------------------
# AC1.2-HP: Late review captured in JSON output
# -------------------------------------------------------------------
test_late_review_in_json() {
    local work
    work=$(mktemp -d -t pmp-test-XXXXXX)
    mkdir -p "$work/.fno"
    echo "99" > "$work/.fno/.memory-pass-pending"

    local stub_dir
    stub_dir=$(mktemp -d -t pmp-stub-XXXXXX)
    make_stub_gh "$stub_dir" "2026-05-05T01:00:00Z" "2026-05-05T02:00:00Z"

    local out rc
    (
        cd "$work"
        git init -q 2>/dev/null || true
        PATH="$stub_dir:$PATH" bash "$PASS_SCRIPT"
    ) > /tmp/pmp_review_out.json 2>/tmp/pmp_review_err.txt
    rc=$?

    out=$(cat /tmp/pmp_review_out.json)

    [[ "$rc" -eq 0 ]] || fail "AC1.2-HP late-review: expected exit 0, got $rc (stderr: $(cat /tmp/pmp_review_err.txt))"

    echo "$out" | python3 -c "import json,sys; d=json.load(sys.stdin); assert len(d['late_reviews']) >= 1, 'expected at least 1 late review, got: ' + str(d['late_reviews'])" \
        || fail "AC1.2-HP late-review: late_reviews should have at least 1 entry. Output: $out"

    pass "AC1.2-HP: late review (designer critique) captured in JSON output"

    rm -rf "$work" "$stub_dir" /tmp/pmp_review_out.json /tmp/pmp_review_err.txt
}

# -------------------------------------------------------------------
# AC1.2-EDGE: No late signal -> empty arrays, sentinel removed, exit 0
# -------------------------------------------------------------------
test_no_late_signal_empty_arrays() {
    local work
    work=$(mktemp -d -t pmp-test-XXXXXX)
    mkdir -p "$work/.fno"
    echo "7" > "$work/.fno/.memory-pass-pending"

    local stub_dir
    stub_dir=$(mktemp -d -t pmp-stub-XXXXXX)
    make_stub_gh_no_late_signal "$stub_dir" "2026-05-05T01:00:00Z"

    local out rc
    (
        cd "$work"
        git init -q 2>/dev/null || true
        PATH="$stub_dir:$PATH" bash "$PASS_SCRIPT"
    ) > /tmp/pmp_empty_out.json 2>/tmp/pmp_empty_err.txt
    rc=$?

    out=$(cat /tmp/pmp_empty_out.json)

    [[ "$rc" -eq 0 ]] || fail "AC1.2-EDGE no-late-signal: expected exit 0, got $rc (stderr: $(cat /tmp/pmp_empty_err.txt))"

    echo "$out" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['late_comments'] == [], 'expected empty late_comments'; assert d['late_reviews'] == [], 'expected empty late_reviews'" \
        || fail "AC1.2-EDGE no-late-signal: expected empty arrays. Output: $out"

    [[ ! -f "$work/.fno/.memory-pass-pending" ]] \
        || fail "AC1.2-EDGE no-late-signal: sentinel should be removed"

    pass "AC1.2-EDGE: no late signal -> empty arrays, sentinel removed, exit 0"

    rm -rf "$work" "$stub_dir" /tmp/pmp_empty_out.json /tmp/pmp_empty_err.txt
}

# -------------------------------------------------------------------
# AC1.2-FR: Second invocation after sentinel gone -> exit 0 silently (one-shot)
# -------------------------------------------------------------------
test_second_invocation_silent() {
    local work
    work=$(mktemp -d -t pmp-test-XXXXXX)
    mkdir -p "$work/.fno"
    # No sentinel = simulates post-first-run state.

    local stub_dir
    stub_dir=$(mktemp -d -t pmp-stub-XXXXXX)
    make_stub_gh "$stub_dir"

    local out rc
    (
        cd "$work"
        git init -q 2>/dev/null || true
        PATH="$stub_dir:$PATH" bash "$PASS_SCRIPT" 2>/dev/null
    ) > /tmp/pmp_second_out.txt
    rc=$?
    out=$(cat /tmp/pmp_second_out.txt)

    [[ "$rc" -eq 0 ]] || fail "AC1.2-FR second-invocation: expected exit 0, got $rc"
    [[ -z "$out" ]] || fail "AC1.2-FR second-invocation: expected empty output (no sentinel), got: $out"
    pass "AC1.2-FR: second invocation after sentinel removal -> exit 0 silently (one-shot)"

    rm -rf "$work" "$stub_dir" /tmp/pmp_second_out.txt
}

# -------------------------------------------------------------------
# AC1.2-HP: done-with-concerns artifact discovered (third explicit signal source)
# -------------------------------------------------------------------
test_done_with_concerns_in_json() {
    local work
    work=$(mktemp -d -t pmp-test-XXXXXX)
    mkdir -p "$work/.fno/artifacts"
    echo "55" > "$work/.fno/.memory-pass-pending"
    # Seed a sigma-review artifact with done-with-concerns verdict.
    cat > "$work/.fno/artifacts/review-test-001.md" <<'ART'
---
phase: review
session_id: test-001
verdict: done-with-concerns
approved: false
---
# Sigma-Review Artifact
ART

    local stub_dir
    stub_dir=$(mktemp -d -t pmp-stub-XXXXXX)
    make_stub_gh "$stub_dir" "2026-05-05T01:00:00Z" "2026-05-05T02:00:00Z" "MERGED"

    local out rc
    (
        cd "$work"
        git init -q 2>/dev/null || true
        PATH="$stub_dir:$PATH" bash "$PASS_SCRIPT"
    ) > /tmp/pmp_dwc_out.json 2>/tmp/pmp_dwc_err.txt
    rc=$?

    out=$(cat /tmp/pmp_dwc_out.json)

    [[ "$rc" -eq 0 ]] || fail "AC1.2-HP done-with-concerns: expected exit 0, got $rc (stderr: $(cat /tmp/pmp_dwc_err.txt))"

    echo "$out" | python3 -c "import json,sys; d=json.load(sys.stdin); assert len(d['done_with_concerns']) >= 1, 'expected at least 1 done_with_concerns path, got: ' + str(d['done_with_concerns']); assert any('review-test-001.md' in p for p in d['done_with_concerns']), 'expected fixture path in list'" \
        || fail "AC1.2-HP done-with-concerns: artifact path missing from JSON. Output: $out"

    pass "AC1.2-HP: done-with-concerns artifact captured in JSON"

    rm -rf "$work" "$stub_dir" /tmp/pmp_dwc_out.json /tmp/pmp_dwc_err.txt
}

# -------------------------------------------------------------------
# AC1.2-FR: PR state OPEN (queued auto-merge) -> sentinel PRESERVED for retry
# This is the dominant target auto-merge path; sentinel must survive until
# the server-side merge actually lands.
# -------------------------------------------------------------------
test_queued_pr_preserves_sentinel() {
    local work
    work=$(mktemp -d -t pmp-test-XXXXXX)
    mkdir -p "$work/.fno"
    echo "200" > "$work/.fno/.memory-pass-pending"

    local stub_dir
    stub_dir=$(mktemp -d -t pmp-stub-XXXXXX)
    # PR is OPEN (queued for auto-merge), mergedAt is null.
    make_stub_gh "$stub_dir" "null" "2026-05-05T02:00:00Z" "OPEN"

    local rc
    (
        cd "$work"
        git init -q 2>/dev/null || true
        PATH="$stub_dir:$PATH" bash "$PASS_SCRIPT" 2>/tmp/pmp_queued_err.txt
    ) > /tmp/pmp_queued_out.txt
    rc=$?

    [[ "$rc" -eq 0 ]] || fail "queued-pr: expected exit 0 (graceful no-op), got $rc"

    # Sentinel MUST still exist; the pass is supposed to retry next time.
    [[ -f "$work/.fno/.memory-pass-pending" ]] \
        || fail "queued-pr: sentinel was removed; should be preserved for retry on queued auto-merge"
    pass "queued-pr: OPEN state preserves sentinel for retry (dominant auto-merge path)"

    rm -rf "$work" "$stub_dir" /tmp/pmp_queued_out.txt /tmp/pmp_queued_err.txt
}

# -------------------------------------------------------------------
# AC1.2-FR: PR state CLOSED (no merge) -> sentinel REMOVED, exit 0 silently.
# -------------------------------------------------------------------
test_closed_pr_cleans_sentinel() {
    local work
    work=$(mktemp -d -t pmp-test-XXXXXX)
    mkdir -p "$work/.fno"
    echo "300" > "$work/.fno/.memory-pass-pending"

    local stub_dir
    stub_dir=$(mktemp -d -t pmp-stub-XXXXXX)
    make_stub_gh "$stub_dir" "null" "2026-05-05T02:00:00Z" "CLOSED"

    local rc
    (
        cd "$work"
        git init -q 2>/dev/null || true
        PATH="$stub_dir:$PATH" bash "$PASS_SCRIPT" 2>/dev/null
    ) > /dev/null
    rc=$?

    [[ "$rc" -eq 0 ]] || fail "closed-pr: expected exit 0, got $rc"
    [[ ! -f "$work/.fno/.memory-pass-pending" ]] \
        || fail "closed-pr: sentinel should be cleaned up for CLOSED PRs"

    pass "closed-pr: CLOSED state cleans up sentinel"

    rm -rf "$work" "$stub_dir"
}

# -------------------------------------------------------------------
# Run all tests
# -------------------------------------------------------------------
test_no_sentinel_exits_silently
test_empty_sentinel_cleaned_up
test_late_comment_in_json
test_late_review_in_json
test_no_late_signal_empty_arrays
test_second_invocation_silent
test_done_with_concerns_in_json
test_queued_pr_preserves_sentinel
test_closed_pr_cleans_sentinel

echo ""
if [[ "$FAILURES" -eq 0 ]]; then
    echo "ALL TESTS PASSED (test_post_merge_pass.sh)"
else
    echo "FAILED: $FAILURES test(s) failed" >&2
    exit 1
fi
