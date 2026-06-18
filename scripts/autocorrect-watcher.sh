#!/usr/bin/env bash
# autocorrect-watcher.sh - poll corrections.log for unprocessed S0 events and
# fire an immediate review.
#
# Scheduled via launchd StartInterval=900 (every 15 minutes) by
# scripts/install-autocorrect-cron.sh. The watcher catches S0 entries that the
# inline trigger in corrections-verifier-log.sh missed - e.g., entries written
# by a future capture path that didn't dispatch.
#
# Idempotency: a per-event hash is recorded in ~/.claude/.s0-processed.log
# after a successful dispatch. Subsequent runs skip any S0 entry already in
# the processed log.
#
# Throttling: if the same source+location+details triple has fired within the
# last 60 minutes, batches them into a single review rather than dispatching
# N reviews.
#
# Failure handling: a failed review (non-zero exit from autocorrect-review.sh)
# does NOT mark the event as processed. The next watcher tick will retry.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
source "$SCRIPT_DIR/lib/corrections-lock.sh"

CLAUDE_DIR="${CLAUDE_DIR_OVERRIDE:-$HOME/.claude}"
LOG_PATH="$(corrections_log_path)"
PROCESSED_LOG="$CLAUDE_DIR/.s0-processed.log"
WATERMARK_PATH="$CLAUDE_DIR/.s0-watcher-watermark"
REVIEW_SCRIPT="$SCRIPT_DIR/autocorrect-review.sh"
PACK_SCRIPT="$SCRIPT_DIR/autocorrect-pack.sh"

# Per-event dedupe via .s0-processed.log is the only throttle in v0;
# the same source+location triple appearing multiple times in the log
# only triggers one dispatch because they share a hash bucket.

DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
  esac
done

if [[ ! -f "$LOG_PATH" ]]; then
  exit 0  # no log, nothing to watch
fi
if [[ ! -x "$REVIEW_SCRIPT" || ! -x "$PACK_SCRIPT" ]]; then
  echo "autocorrect-watcher: review or pack script not executable; nothing to do" >&2
  exit 0
fi

hash_of() {
  printf '%s' "$1" | md5 -q 2>/dev/null || printf '%s' "$1" | md5sum | awk '{print $1}'
}

hash_processed() {
  [[ -f "$PROCESSED_LOG" ]] || return 1
  grep -Fxq "$1" "$PROCESSED_LOG" 2>/dev/null
}

# Collect all S0 entries that aren't already processed.
UNPROCESSED_HASHES=()
UNPROCESSED_LINES=()
while IFS= read -r line; do
  [[ -z "$line" ]] && continue
  sev=$(printf '%s' "$line" | awk -F' \\| ' '{print $2}')
  [[ "$sev" != "S0" ]] && continue
  source_field=$(printf '%s' "$line" | awk -F' \\| ' '{print $3}')
  location=$(printf '%s' "$line" | awk -F' \\| ' '{print $4}')
  details=$(printf '%s' "$line" | awk -F' \\| ' '{print $5}')
  entry_key="${source_field}|${location}|${details}"
  h="$(hash_of "$entry_key")"
  if hash_processed "$h"; then
    continue
  fi
  UNPROCESSED_HASHES+=("$h")
  UNPROCESSED_LINES+=("$line")
done < "$LOG_PATH"

if [[ "${#UNPROCESSED_HASHES[@]}" -eq 0 ]]; then
  exit 0
fi

echo "autocorrect-watcher: ${#UNPROCESSED_HASHES[@]} unprocessed S0 entry(ies); dispatching review" >&2

if [[ "$DRY_RUN" == "1" ]]; then
  for line in "${UNPROCESSED_LINES[@]}"; do
    printf 'would-dispatch: %s\n' "$line"
  done
  exit 0
fi

# Build packet and review. Stderr from pack is forwarded to ours so a real
# pack failure (malformed timestamp, missing helpers, etc.) surfaces in the
# launchd-captured StandardErrorPath log rather than getting swallowed.
if ! PACKET=$("$PACK_SCRIPT" --severity S0 --dry-run); then
  echo "autocorrect-watcher: pack failed" >&2
  exit 1
fi

if ! printf '%s\n' "$PACKET" | "$REVIEW_SCRIPT" --severity S0 --packet-source watcher; then
  echo "autocorrect-watcher: review failed; entries remain unprocessed for retry" >&2
  exit 1
fi

# Mark all dispatched events processed.
for h in "${UNPROCESSED_HASHES[@]}"; do
  printf '%s\n' "$h" >> "$PROCESSED_LOG"
done
chmod 600 "$PROCESSED_LOG" 2>/dev/null || true

# Update watermark.
date -u +"%Y-%m-%dT%H:%M:%SZ" > "$WATERMARK_PATH"
chmod 600 "$WATERMARK_PATH" 2>/dev/null || true

exit 0
