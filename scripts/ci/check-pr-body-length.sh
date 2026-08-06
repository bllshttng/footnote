#!/usr/bin/env bash
# check-pr-body-length.sh - CI gate: keep PR bodies short.
#
# A PR body is a relay surface: re-read by every reviewer and every CI re-run.
# Fails when the body has more than MAX_LINES non-empty lines (fenced code
# blocks excluded), unless the body carries a brevity-exception: line with a
# non-empty rationale. Mirrors the loc-exception escape shape.
#
# Run: PR_BODY="<body>" bash scripts/ci/check-pr-body-length.sh
# Env: PR_BODY (the PR body), PR_BODY_MAX_LINES (default 15).
# Exit: 0 pass or skip, 1 over cap.

set -euo pipefail

MAX_LINES="${PR_BODY_MAX_LINES:-15}"
PR_BODY="${PR_BODY:-}"

# Fail-open: no body set (local run, or a draft with no body yet).
if [[ -z "$PR_BODY" ]]; then
  echo "check-pr-body-length: no PR_BODY set, skipping."
  exit 0
fi

# Exception hatch: a brevity-exception: line with non-empty rationale.
if printf '%s\n' "$PR_BODY" | grep -qE '^brevity-exception:[[:space:]]*[^[:space:]]'; then
  echo "check-pr-body-length: brevity-exception declared, skipping."
  exit 0
fi

# Non-empty lines, excluding fenced code blocks (``` ... ```).
count=$(printf '%s\n' "$PR_BODY" | awk '
  { sub(/\r$/, "") }
  /^```/ { in_fence = !in_fence; next }
  in_fence { next }
  NF > 0 { c++ }
  END { print c + 0 }
')

if (( count > MAX_LINES )); then
  {
    echo "check-pr-body-length: ${count} non-empty lines (cap ${MAX_LINES}, code fences excluded)."
    echo "  A PR body is re-read by every reviewer. Trim it, or add a line:"
    echo "    brevity-exception: <rationale>"
  } >&2
  exit 1
fi

echo "check-pr-body-length: ${count} non-empty lines (cap ${MAX_LINES})."
