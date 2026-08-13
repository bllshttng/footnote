#!/usr/bin/env bash
# check-pr-style.sh - CI gate: a PR body must pass the six style rules.
#
# A PR body is the first thing a reviewer reads. An agent that learns the rule
# only from a mail refusal pays a full CI round trip to find out, so the same
# six rules run here, at the late gate. Mirrors check-pr-body-length.sh: fail
# open when PR_BODY is unset (a local run or a draft with no body yet).
#
# The escape is a `style-exception:` line with a non-empty reason, handled by
# the verb itself (it scans stdin for the marker).
#
# Run: PR_BODY="<body>" bash scripts/ci/check-pr-style.sh
# Exit: 0 pass or skip, 1 a style violation, 2 the checker could not run.

set -euo pipefail

PR_BODY="${PR_BODY:-}"

if [[ -z "$PR_BODY" ]]; then
  echo "check-pr-style: no PR_BODY set, skipping."
  exit 0
fi

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$REPO_ROOT/cli"

# Prefer the in-tree build (`uv run`) over an installed snapshot: a stale global
# `fno` may predate the `lint style` verb (install staleness).
if command -v uv >/dev/null 2>&1; then
  FNO=(uv run fno-py)
elif command -v fno >/dev/null 2>&1; then
  FNO=(fno)
else
  echo "check-pr-style: neither 'uv' nor 'fno' available" >&2
  exit 2
fi

if printf '%s' "$PR_BODY" | "${FNO[@]}" lint style --surface pr-body --stdin; then
  echo "check-pr-style: ok"
  exit 0
fi
echo "check-pr-style: PR body violates the style rules listed above." >&2
exit 1
