#!/usr/bin/env bash
# Keep the authoring Codex session working when its native review returns findings.
# This is a local Stop-hook nudge: the review output is already in the worker's
# context, so no king, daemon notification, or external reader belongs here.
set -euo pipefail

input="$(cat)"
event="$(printf '%s' "$input" | jq -r '.hook_event_name // empty' 2>/dev/null || true)"
[[ "$event" == "Stop" ]] || exit 0

turn_id="$(printf '%s' "$input" | jq -r '.turn_id // empty' 2>/dev/null || true)"
transcript="$(printf '%s' "$input" | jq -r '.transcript_path // empty' 2>/dev/null || true)"
[[ -n "$turn_id" && -n "$transcript" ]] || exit 0
transcript="${transcript/#\~/$HOME}"
[[ -r "$transcript" ]] || exit 0

review_outputs="$(jq -s --arg turn "$turn_id" '
  [
    .[]
    | select(.type == "event_msg" and .payload.turn_id == $turn)
    | if (.payload.type == "item_completed"
          and .payload.item.type == "ExitedReviewMode") then
        .payload.item.review_output
      elif .payload.type == "exited_review_mode" then
        .payload.review_output
      else
        empty
      end
  ]
' "$transcript" 2>/dev/null)" || exit 0

# Require exactly one same-turn structured completion and a present non-empty
# findings array. A missing marker, duplicate completion, or malformed row is
# not permission to interrupt the worker.
findings="$(jq -c '
  if length == 1
     and (.[0] | type == "object" and has("findings")
          and (.findings | type == "array") and (.findings | length > 0))
  then .[0].findings
  else empty
  end
' <<<"$review_outputs" 2>/dev/null)" || exit 0
[[ -n "$findings" ]] || exit 0

repo_root="$(git rev-parse --show-toplevel 2>/dev/null || printf '%s' "$PWD")"
safe_turn="$(printf '%s' "$turn_id" | tr -cd 'A-Za-z0-9._-')"
[[ -n "$safe_turn" ]] || exit 0
marker_dir="$repo_root/.fno/scratchpad/codex-review-findings"
marker="$marker_dir/$safe_turn.nudged"
mkdir -p "$marker_dir" 2>/dev/null || exit 0
if [[ -e "$marker" ]]; then
  exit 0
fi
touch "$marker" 2>/dev/null || exit 0

total="$(jq 'length' <<<"$findings" 2>/dev/null || printf 'unknown')"
p1="$(jq '[.[] | (.priority // .severity // "") | ascii_upcase | select(. == "P1")] | length' <<<"$findings" 2>/dev/null || printf '0')"
p2="$(jq '[.[] | (.priority // .severity // "") | ascii_upcase | select(. == "P2")] | length' <<<"$findings" 2>/dev/null || printf '0')"

cat >&2 <<EOF
Native review returned $total finding(s) for this turn (P1: $p1, P2: $p2). Act on the findings in your current context now. Read each finding, fix actionable P1/P2 issues, run focused tests, commit and push. Then run 'fno do target request-self-review --pr <n>' on the NEW HEAD; the old attestation is stale. Do not promise completion while findings remain.
EOF
exit 2
