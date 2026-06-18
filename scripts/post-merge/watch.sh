#!/usr/bin/env bash
# watch.sh - poll merged PRs for THIS repo and fire the post-merge ritual
# headlessly for each PR merged since the watermark. Catches web-button merges
# (a GitHub-UI merge produces no local event, so /fno:pr merged never
# fires for it). Phase 2 of the post-merge ritual (ab-4e9fb05a).
#
# Intended to run on an interval via a per-repo macOS LaunchAgent
# (com.fno.postmerge.<repo>.plist; see install.sh). launchd runs this off
# the user's interactive path, so the user is never blocked. Each ritual fire
# is run SYNCHRONOUSLY (waited) - that is deliberate, not a regression of the
# design's "detached" wording: the watermark may advance ONLY after a
# successful fire (AC1-ERR), which requires knowing the fire's exit status, and
# a detached nohup fire cannot report that. The non-blocking guarantee lives at
# the launchd layer, not per-fire.
#
# Watermark: the last-processed PR mergedAt (ISO-8601, lexically sortable for
# UTC). Stored per-repo at .fno/.post-merge-watermark so it rides the
# symlink-per-file worktree model and sits next to the repo's other state.
# Advanced ONLY after a successful fire; a failed fire leaves it so the merge
# is retried next poll.
#
# Env overrides (all optional):
#   POST_MERGE_MODEL         - model for the `claude --print` fire (default
#                              claude-haiku-4-5; empty = inherit CLI default).
#   POST_MERGE_POLL_LIMIT    - gh --limit (default 100).
#   POST_MERGE_PRS_JSON      - JSON array of {number,mergedAt,title}; bypasses gh (tests).
#   POST_MERGE_FIRE_CMD      - command run with the PR number appended instead of
#                              `claude --print ...` (point at `true`/`false` in tests).
#   POST_MERGE_WATERMARK_FILE- override the watermark path (tests).
#
# Exit codes: 0 = all new merges processed (or none); 1 = a fire failed (watermark
# left for retry); 2 = missing dependency (gh / jq) when no test seam supplies data.

set -euo pipefail

# Fall back to a SCRIPT-relative root (this file lives in scripts/post-merge/),
# not pwd: a launchd job or a call from a subdir would otherwise resolve wrong.
REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || (cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd))"
WATERMARK_FILE="${POST_MERGE_WATERMARK_FILE:-${REPO_ROOT}/.fno/.post-merge-watermark}"
# Default 100 (not 20): gh returns newest-first, so a too-small limit drops the
# OLDEST post-watermark merges on a first install or after the watcher was down
# over a busy window, and they would never get the ritual (Codex P1, PR #390).
LIMIT="${POST_MERGE_POLL_LIMIT:-100}"
# Model for the headless ritual fire. The post-merge ritual is largely
# mechanical (close the node, write prose follow-ups, file triage), so it
# defaults to Haiku to keep per-merge cost low. Override with POST_MERGE_MODEL
# (e.g. `sonnet`) if you want stronger triage judgment. Empty value = inherit
# the CLI's default model.
MODEL="${POST_MERGE_MODEL-claude-haiku-4-5}"

log()  { printf 'post-merge watch: %s\n' "$*"; }
warn() { printf 'post-merge watch: %s\n' "$*" >&2; }

# Fire the ritual for one PR. Returns the fire's exit status.
fire_ritual() {
  local pr="$1"
  if [[ -n "${POST_MERGE_FIRE_CMD:-}" ]]; then
    # Test/override hook: word-split intentionally so `POST_MERGE_FIRE_CMD=false`
    # or a multi-word command both work.
    # shellcheck disable=SC2086
    ${POST_MERGE_FIRE_CMD} "$pr"
  else
    # Real fire: run the skill headlessly in the repo, waited so we learn the
    # exit status. The skill itself is idempotent per PR (marker-keyed).
    # Two branches (not a "${arr[@]}" on a possibly-empty array, which errors
    # under bash 3.2 + set -u) so an empty MODEL inherits the CLI default.
    if [[ -n "$MODEL" ]]; then
      ( cd "$REPO_ROOT" && claude --print --dangerously-skip-permissions --model "$MODEL" "/fno:pr merged ${pr}" )
    else
      ( cd "$REPO_ROOT" && claude --print --dangerously-skip-permissions "/fno:pr merged ${pr}" )
    fi
  fi
}

# jq is always needed to parse the PR list (test seam or gh).
command -v jq >/dev/null 2>&1 || { warn "jq not found; cannot parse PRs"; exit 2; }

# Acquire the merged-PR list: from the test seam if present, else gh.
if [[ -n "${POST_MERGE_PRS_JSON:-}" ]]; then
  prs_json="$POST_MERGE_PRS_JSON"
else
  command -v gh >/dev/null 2>&1 || { warn "gh CLI not found; cannot poll"; exit 2; }
  # Distinguish a real fetch failure (auth expiry, network, rate-limit) from a
  # genuinely empty result. The old `... 2>/dev/null || echo '[]'` collapsed
  # both into "no merges", so an expired token would make the watcher silently
  # stop catching merges while logging a clean exit 0 (silent-failure review).
  # Capture gh's stderr in a temp FILE, not 2>&1: folding stderr into stdout
  # would pollute prs_json with any gh diagnostic chatter (update notices) and
  # break jq (Gemini PR #390). The real error surfaces only on the failure path.
  gh_err="$(mktemp)"
  if ! prs_json="$(gh pr list --state merged --json number,mergedAt,title --limit "$LIMIT" 2>"$gh_err")"; then
    warn "gh pr list failed (auth/network/rate-limit?): $(cat "$gh_err")"
    rm -f "$gh_err"
    exit 3
  fi
  rm -f "$gh_err"
fi

# Read the current watermark. Composite "<mergedAt>\t<num>" so two PRs merged in
# the SAME second are ordered by PR number and neither is skipped on a mid-batch
# failure (code-review finding). Back-compat: a bare timestamp parses with
# wm_num=0. Track whether the file existed so a TRUE first run baselines rather
# than firing the ritual for the repo's whole merge history.
wm_time=""
wm_num="0"
watermark_existed=0
if [[ -f "$WATERMARK_FILE" ]]; then
  watermark_existed=1
  IFS=$'\t' read -r wm_time wm_num < "$WATERMARK_FILE" 2>/dev/null || true
  wm_time="$(printf '%s' "${wm_time:-}" | tr -d '[:space:]')"
  [[ "${wm_num:-}" =~ ^[0-9]+$ ]] || wm_num="0"
fi

# First run (no watermark yet): establish a BASELINE at the newest merged PR
# WITHOUT firing, so installing the watcher does not retroactively run the ritual
# for the entire merge history (Codex P1 "first install"; would otherwise mean
# ~100 claude --print runs + duplicate vault followups on install). Only merges
# AFTER this baseline are processed on subsequent polls.
if [[ "$watermark_existed" -eq 0 ]]; then
  newest="$(printf '%s' "$prs_json" | jq -r '
    (if type=="array" then . else [] end)
    | map(select(.mergedAt != null and .number != null))
    | if length>0 then (sort_by([.mergedAt,.number])|last|"\(.mergedAt)\t\(.number)") else "" end')"
  mkdir -p "$(dirname "$WATERMARK_FILE")" 2>/dev/null || true
  if [[ -n "$newest" ]]; then
    printf '%s' "$newest" > "$WATERMARK_FILE"
    log "first run: baseline established at ${newest%%$'\t'*} (#${newest##*$'\t'}); prior merges are NOT processed retroactively. New merges from here fire the ritual."
  else
    log "first run: no merged PRs found; baseline empty (next merge establishes it)."
  fi
  exit 0
fi

# Truncation guard: gh returns newest-first, so if the fetch HIT the limit AND
# the OLDEST fetched PR is still newer than the watermark, there may be even
# older post-watermark merges beyond the window that advancing would skip. Warn
# loudly (never skip silently) so the operator can raise POST_MERGE_POLL_LIMIT
# (true pagination is a deferred follow-up). Stays quiet when the window already
# reaches back past the watermark - which is the common case (Codex P1).
fetched_count="$(printf '%s' "$prs_json" | jq 'if type=="array" then length else 0 end' 2>/dev/null || echo 0)"
oldest_fetched="$(printf '%s' "$prs_json" | jq -r 'if (type=="array" and length>0) then (map(.mergedAt)|min) else "" end' 2>/dev/null || echo "")"
if [[ "$fetched_count" =~ ^[0-9]+$ ]] && (( fetched_count >= LIMIT )) \
   && [[ -n "$oldest_fetched" && "$oldest_fetched" > "$wm_time" ]]; then
  warn "fetched $fetched_count merged PRs (== limit $LIMIT) and the oldest is still newer than the watermark; OLDER post-watermark merges may be missed - raise POST_MERGE_POLL_LIMIT"
fi

# Select PRs after the composite watermark, oldest-first by (mergedAt, number)
# so the watermark advances monotonically and a mid-batch failure never skips an
# earlier PR - including a same-second sibling. while-read (not mapfile): macOS
# /bin/bash is 3.2 and mapfile is a bash-4 builtin that errors there.
#
# KNOWN LIMITATION (Codex P2, PR #390; deferred follow-up): the number tie-break
# assumes PR-number order == merge order WITHIN a single mergedAt second. PR
# numbers are creation order, so if two PRs merge in the exact same second and
# the later-merged one has a LOWER number, and the poll happens to run between
# them, the lower-numbered later merge is filtered out (.number > wn is false)
# and its ritual is skipped. This requires same-second + reverse-number + poll
# timing, so it is astronomically rare; a fully robust fix tracks the processed
# PR-number SET for the boundary second (deferred - not worth a 4th watermark
# rework on a gated, idempotent-skill feature).
# Capture jq's output first (not `... 2>/dev/null || true` in a process sub):
# under set -euo pipefail a jq parse failure on malformed input then ABORTS the
# poll loudly instead of silently yielding an empty list and a clean exit 0
# (Gemini PR #390). A genuinely empty selection is an empty string -> no rows.
jq_out="$(printf '%s' "$prs_json" | jq -r --arg wt "$wm_time" --argjson wn "$wm_num" '
  (if type == "array" then . else [] end)
  | map(select(.mergedAt != null and .number != null
      and ((.mergedAt > $wt) or (.mergedAt == $wt and .number > $wn))))
  | sort_by([.mergedAt, .number])
  | .[] | "\(.mergedAt)\t\(.number)"')"
rows=()
while IFS= read -r _line; do
  [[ -n "$_line" ]] && rows+=("$_line")
done <<< "$jq_out"

if [[ ${#rows[@]} -eq 0 ]]; then
  log "no new merges since ${wm_time:-<beginning>}${wm_time:+ (#$wm_num)}"
  exit 0
fi

# Ensure the watermark's parent dir exists. A fresh clone may have no .fno/;
# without this the post-fire watermark write aborts under set -e and the watcher
# never advances, re-firing forever (code-review finding).
mkdir -p "$(dirname "$WATERMARK_FILE")" 2>/dev/null || true

for row in "${rows[@]}"; do
  IFS=$'\t' read -r merged_at number <<<"$row"
  log "firing ritual for PR #${number} (merged ${merged_at})"
  if fire_ritual "$number"; then
    # Advance the composite watermark ONLY on success (atomic write; clean up the
    # tmp on any write failure rather than littering .fno/).
    tmp="$(mktemp "${WATERMARK_FILE}.XXXXXX" 2>/dev/null || true)"
    if [[ -n "$tmp" ]] && printf '%s\t%s' "$merged_at" "$number" > "$tmp" 2>/dev/null; then
      mv "$tmp" "$WATERMARK_FILE"
    else
      [[ -n "$tmp" ]] && rm -f "$tmp"
      printf '%s\t%s' "$merged_at" "$number" > "$WATERMARK_FILE"
    fi
    log "PR #${number} processed; watermark -> ${merged_at} (#${number})"
  else
    warn "PR #${number} fire FAILED; watermark left at ${wm_time:-<beginning>} (retried next poll)"
    exit 1
  fi
done
