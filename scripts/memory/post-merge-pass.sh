#!/usr/bin/env bash
# post-merge-pass.sh - capture late-arriving PR signal into memory.
# Triggered by .fno/.memory-pass-pending sentinel; one-shot.
#
# Discovers:
#   - PR comments since merged_at
#   - Reviews since merged_at
#   - Sigma-review artifacts with done-with-concerns verdict (ungraduated)
#
# Emits JSON to stdout. The CALLER (a main-thread LLM step in /pr check or
# the stop hook) decides what is memory-worthy and calls write-memory-entry.sh.
# This script DOES NOT call claude -p; it just discovers signal.
#
# Sentinel handling:
#   - PR state MERGED: discovery runs, sentinel removed on success.
#   - PR state OPEN (queued auto-merge): sentinel KEPT for retry; exit 0 silently.
#   - PR state CLOSED (no merge): sentinel removed; nothing to capture.
#   - gh CLI absent / not in repo: sentinel removed; pass is a no-op here.
#
# Exit codes:
#   0  success or graceful no-op (caller proceeds either way)
#   2  partial failure: discovery hit a recoverable error AND sentinel was
#      preserved so the next invocation can retry. Stop hook + check-pr
#      treat 2 as a soft failure worth logging but never blocking.

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
SENTINEL="$REPO_ROOT/.fno/.memory-pass-pending"

[[ -f "$SENTINEL" ]] || exit 0
PR_NUMBER=$(cat "$SENTINEL" 2>/dev/null | head -1 | tr -d '[:space:]')
[[ -z "$PR_NUMBER" ]] && { rm -f "$SENTINEL"; exit 0; }

# Verify gh is installed and we are in a github repo.
command -v gh >/dev/null 2>&1 || {
    echo "post-merge-pass: gh CLI not available" >&2
    rm -f "$SENTINEL"
    exit 0
}

OWNER=$(gh repo view --json owner --jq '.owner.login' 2>/dev/null) || OWNER=""
REPO=$(gh repo view --json name --jq '.name' 2>/dev/null) || REPO=""
[[ -z "$OWNER" || -z "$REPO" ]] && {
    echo "post-merge-pass: not in a github repo" >&2
    rm -f "$SENTINEL"
    exit 0
}

# Single gh call for state + mergedAt so we can decide whether to keep the
# sentinel for retry (queued auto-merge) or clean it up (closed-without-merge).
PR_META=$(gh pr view "$PR_NUMBER" --json state,mergedAt 2>/dev/null) || PR_META=""
if [[ -z "$PR_META" ]]; then
    # gh API failed transiently. Preserve sentinel so the next invocation
    # can retry. Exit 2 so the caller can log a soft failure.
    echo "post-merge-pass: gh pr view failed for PR $PR_NUMBER; sentinel preserved" >&2
    exit 2
fi
PR_STATE=$(printf '%s' "$PR_META" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("state",""))' 2>/dev/null) || PR_STATE=""
MERGED_AT=$(printf '%s' "$PR_META" | python3 -c 'import sys,json; v=json.load(sys.stdin).get("mergedAt"); print(v if v else "")' 2>/dev/null) || MERGED_AT=""

case "$PR_STATE" in
    MERGED)
        : # proceed with discovery below
        ;;
    OPEN)
        # Queued auto-merge or just open. KEEP the sentinel - the merge has
        # not landed yet. Exit 0 so the caller treats this as graceful.
        echo "post-merge-pass: PR $PR_NUMBER is OPEN (state=$PR_STATE); sentinel preserved" >&2
        exit 0
        ;;
    CLOSED|*)
        # Closed without merge, or unknown state. Nothing to capture.
        echo "post-merge-pass: PR $PR_NUMBER state=$PR_STATE (not merged); cleaning up sentinel" >&2
        rm -f "$SENTINEL"
        exit 0
        ;;
esac

if [[ -z "$MERGED_AT" || "$MERGED_AT" == "null" ]]; then
    # MERGED state but no mergedAt - shouldn't happen, but be defensive.
    echo "post-merge-pass: PR $PR_NUMBER state=MERGED but no mergedAt; sentinel preserved" >&2
    exit 2
fi

# Late comments (post-merge): single page is sufficient for the post-merge
# window in practice. --paginate concatenates multiple JSON arrays per page,
# which the Python parser below cannot json.loads correctly. We track
# whether the gh call succeeded so partial failures are surfaced.
LC_OK=true
LATE_COMMENTS=$(gh api "repos/$OWNER/$REPO/issues/$PR_NUMBER/comments" \
    --jq "[.[] | select(.created_at > \"$MERGED_AT\") | {user: .user.login, body: .body, created_at: .created_at}]" 2>/dev/null) \
    || { LC_OK=false; LATE_COMMENTS="[]"; }
[[ -z "$LATE_COMMENTS" ]] && LATE_COMMENTS="[]"

LR_OK=true
LATE_REVIEWS=$(gh api "repos/$OWNER/$REPO/pulls/$PR_NUMBER/reviews" \
    --jq "[.[] | select(.submitted_at > \"$MERGED_AT\") | {user: .user.login, body: .body, state: .state, submitted_at: .submitted_at}]" 2>/dev/null) \
    || { LR_OK=false; LATE_REVIEWS="[]"; }
[[ -z "$LATE_REVIEWS" ]] && LATE_REVIEWS="[]"

# Ungraduated done-with-concerns sigma-review artifacts.
DONE_WITH_CONCERNS_FILES=$(grep -lE '^verdict: done-with-concerns' "$REPO_ROOT"/.fno/artifacts/review-*.md 2>/dev/null | tr '\n' ',' | sed 's/,$//') || DONE_WITH_CONCERNS_FILES=""
DWC_JSON=$(printf '%s' "$DONE_WITH_CONCERNS_FILES" | python3 -c 'import sys, json; s=sys.stdin.read().strip(); print(json.dumps([p for p in s.split(",") if p]))' 2>/dev/null) || DWC_JSON="[]"

# Emit the JSON. If the Python heredoc fails, KEEP the sentinel so the next
# invocation can retry. Use a tempfile so we know whether stdout was emitted.
PY_OUT=$(mktemp -t post-merge-pass-out-XXXXXX) || {
    echo "post-merge-pass: mktemp failed; sentinel preserved" >&2
    exit 2
}
PR_NUMBER="$PR_NUMBER" MERGED_AT="$MERGED_AT" LC="$LATE_COMMENTS" LR="$LATE_REVIEWS" DWC="$DWC_JSON" \
LC_OK="$LC_OK" LR_OK="$LR_OK" \
    python3 - <<'PYEOF' > "$PY_OUT"
import os, json, sys
out = {
    "pr": int(os.environ["PR_NUMBER"]),
    "merged_at": os.environ["MERGED_AT"],
    "late_comments": json.loads(os.environ["LC"]) if os.environ.get("LC") else [],
    "late_reviews": json.loads(os.environ["LR"]) if os.environ.get("LR") else [],
    "done_with_concerns": json.loads(os.environ["DWC"]) if os.environ.get("DWC") else [],
    "comments_fetch_ok": os.environ.get("LC_OK") == "true",
    "reviews_fetch_ok": os.environ.get("LR_OK") == "true",
}
print(json.dumps(out))
PYEOF
PY_RC=$?
if [[ $PY_RC -ne 0 ]]; then
    echo "post-merge-pass: JSON encode failed (rc=$PY_RC); sentinel preserved" >&2
    rm -f "$PY_OUT"
    exit 2
fi

cat "$PY_OUT"
rm -f "$PY_OUT"

# Only remove the sentinel when discovery actually succeeded. Partial gh
# failures keep the sentinel for retry on the next invocation.
if [[ "$LC_OK" == "true" && "$LR_OK" == "true" ]]; then
    rm -f "$SENTINEL"
    exit 0
else
    echo "post-merge-pass: partial gh API failure (comments=$LC_OK reviews=$LR_OK); sentinel preserved for retry" >&2
    exit 2
fi
