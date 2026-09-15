#!/usr/bin/env bash
# Put the live laws that govern a review beside the review as it starts.
# The matcher and the index read live in `fno-agents law-match` (mode stage).
set -uo pipefail
command -v fno-agents >/dev/null 2>&1 || exit 0
command -v jq >/dev/null 2>&1 || exit 0
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../scripts/lib/with-timeout.sh
source "$HOOK_DIR/../scripts/lib/with-timeout.sh" 2>/dev/null || exit 0
input="$(cat 2>/dev/null || true)"
[[ -n "$input" ]] || exit 0
printf '%s' "$input" | jq -c '{mode: "stage", hook: .}' 2>/dev/null \
  | with_timeout 5 fno-agents law-match 2>/dev/null \
  | jq -c '.hook_output // empty' 2>/dev/null
exit 0
