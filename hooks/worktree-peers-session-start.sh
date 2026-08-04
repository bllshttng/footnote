#!/usr/bin/env bash
# Shared SessionStart carrier for the worktree peer overlap advisory.
#
# One path for both harness manifests: Claude registers this carrier directly
# (hooks.json); the Codex wrapper (hooks/session-start.sh) calls it with
# --body-only and folds the result into its combined context.
#
# The shared predicate (hooks/helpers/worktree-live-peers.sh) is evaluated ONCE
# in --machine mode. The advisory text is rendered from that result, then the
# same observation object is best-effort recorded via `fno worktree
# overlap-record` and the recurrence window folded. Advisory-only and always
# exits 0: a missing CLI, a rejected payload, lock contention, an unwritable
# journal, or a degraded fold never blocks the session and never refuses a tool
# call - a failure surfaces a visible marker instead ([fno-overlap-unrecorded]
# or [fno-overlap-count-unavailable]).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BODY_ONLY=0
[[ "${1:-}" == "--body-only" ]] && BODY_ONLY=1

# Bound shelling out so a hung CLI or a slow fold over a huge journal can never
# block SessionStart. The helper ships beside this hook; if absent (should not
# happen in a real install), calls fall back to unbounded best-effort.
WT_HELPER="$SCRIPT_DIR/../scripts/lib/with-timeout.sh"
if [[ -f "$WT_HELPER" ]]; then
  # shellcheck source=/dev/null
  source "$WT_HELPER"
fi

# Evaluate the predicate once in machine mode. Empty output means no fresh peer
# (self-only, stale, missing identity, missing/malformed dir) -> stay silent.
obs="$(bash "$SCRIPT_DIR/helpers/worktree-live-peers.sh" --machine 2>/dev/null || true)"
[[ -n "$obs" ]] || exit 0

lines=('- Another session is working in this worktree. [fno-overlap-observed]')

# Best-effort record + recurrence fold. A missing/old fno, a rejected payload,
# lock contention, an unreadable journal, OR a hung CLI never changes the
# advisory and never makes this hook exit nonzero. The fno call is wall-clock
# bounded (with_timeout, 30s) so a slow fold cannot block SessionStart; a timeout
# surfaces as unrecorded. jq is a SessionStart dependency (the platform wrappers
# already require it); if it is somehow absent we cannot confirm the recording
# and surface unrecorded rather than guessing.
record_status=""
if command -v fno >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
  raw=""
  raw_rc=0
  if command -v with_timeout >/dev/null 2>&1; then
    raw="$(printf '%s' "$obs" | with_timeout 30 fno worktree overlap-record --stdin --since 28 2>/dev/null)"; raw_rc=$?
  else
    # No wall-clock bound available: never make an unbounded call that could
    # block SessionStart. Surface unrecorded; the advisory still prints.
    record_status="unrecorded"
  fi
  if [[ -z "$record_status" ]]; then
    if (( raw_rc == 124 )); then
      # The bound fired: treat as unrecorded, never block SessionStart.
      record_status="unrecorded"
    else
      recorded="$(printf '%s' "$raw" | jq -r 'if (.recorded|type)=="boolean" then .recorded else empty end' 2>/dev/null || true)"
      if [[ "$recorded" == "true" ]]; then
        fold_state="$(printf '%s' "$raw" | jq -r '.fold.state // empty' 2>/dev/null || true)"
        if [[ "$fold_state" != "complete" && "$fold_state" != "no_data" ]]; then
          # Recorded, but the recurrence fold could not be trusted.
          record_status="count-unavailable"
        else
          met="$(printf '%s' "$raw" | jq -r '.fold.recurrence_threshold_met // false' 2>/dev/null || true)"
          if [[ "$met" == "true" ]]; then
            distinct="$(printf '%s' "$raw" | jq -r '.fold.distinct_observations // 0' 2>/dev/null || echo 0)"
            thresh="$(printf '%s' "$raw" | jq -r '.fold.recurrence_threshold // 3' 2>/dev/null || echo 3)"
            lines+=("- recurrence reached ${distinct}/${thresh} in 28 days; run \`fno worktree overlaps --since 28\`. A Stage 3 worktree-write-lock design node is now warranted (not filed automatically).")
          fi
        fi
      else
        record_status="unrecorded"
      fi
    fi
  fi
else
  record_status="unrecorded"
fi

case "$record_status" in
  unrecorded) lines+=('- [fno-overlap-unrecorded]') ;;
  count-unavailable) lines+=('- [fno-overlap-count-unavailable]') ;;
esac

if (( BODY_ONLY )); then
  printf '%s\n' "${lines[@]}"
else
  printf '## Worktree hygiene\n'
  printf '%s\n' "${lines[@]}"
fi
exit 0
