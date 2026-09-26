#!/usr/bin/env bash
# Put the live laws that govern a review beside the review as it starts.
# The matcher and the index read live in `fno inbox law stage` (mode stage).
set -uo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH
command -v fno >/dev/null 2>&1 || exit 0
command -v jq >/dev/null 2>&1 || exit 0
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../scripts/lib/with-timeout.sh
source "$HOOK_DIR/../scripts/lib/with-timeout.sh" 2>/dev/null || exit 0
# shellcheck source=lib/write-targets.sh
source "$HOOK_DIR/lib/write-targets.sh" 2>/dev/null || true
input="$(cat 2>/dev/null || true)"
[[ -n "$input" ]] || exit 0
# An Edit|Write payload carries the files it is about to change; they ride
# the request as `paths` and the verb answers the edit read (path laws) in
# place of the verb classifier. A prompt or Skill payload builds the request
# exactly as before, with no `paths` key.
file_path="$(printf '%s' "$input" | jq -r '.tool_input.file_path // empty' 2>/dev/null || true)"
command_str="$(printf '%s' "$input" | jq -r '.tool_input.command // empty' 2>/dev/null || true)"
targets="$(write_targets "$file_path" "$command_str" 2>/dev/null || true)"
request=""
if [[ -n "$targets" ]]; then
  paths_json="$(printf '%s\n' "$targets" | jq -R . | jq -s -c . 2>/dev/null || true)"
  request="$(printf '%s' "$input" | jq -c --argjson paths "$paths_json" '{mode: "stage", hook: ., paths: $paths}' 2>/dev/null || true)"
else
  request="$(printf '%s' "$input" | jq -c '{mode: "stage", hook: .}' 2>/dev/null || true)"
fi
printf '%s' "$request" | with_timeout 5 fno inbox law stage 2>/dev/null \
  | jq -c '.hook_output // empty' 2>/dev/null
exit 0
