#!/usr/bin/env bash
# corrections-insights-tag.sh - ingest the /fno:intel report's operator
# corrections into corrections.log as S2 events.
#
# /fno:intel step 5 passes its report here (skills/intel/SKILL.md): the
# "Operator corrections" lines end in #agent-correction and carry the
# correction text in double quotes plus a signal=<category> pair.
#
# Tracks a watermark via a content hash so re-runs do not double-ingest.
# Watermark file: ~/.fno/corrections.log.wm (line-delimited, one hash per line).
#
# Exit 0 on a graceful no-op. Exit 2 when --insights-file is missing.
# Exit 1 on actual errors (lock failure, missing path, etc).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
source "$SCRIPT_DIR/lib/corrections-lock.sh"

INSIGHTS_FILE_ARG=""
DRY_RUN=0
TAG_PATTERN="#agent-correction"

usage() {
  cat >&2 <<'EOF'
Usage: corrections-insights-tag.sh --insights-file <path> [--dry-run]

Reads the /fno:intel report and emits S2 corrections.log entries for every
line containing #agent-correction. Watermark prevents re-ingestion of
already-seen entries.

Options:
  --insights-file <path>   the /fno:intel report to read (required)
  --dry-run                print would-emit lines to stdout, do not write log
EOF
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --insights-file) INSIGHTS_FILE_ARG="${2:-}"; shift 2 ;;
    --dry-run)       DRY_RUN=1; shift ;;
    -h|--help)       usage ;;
    *) echo "corrections-insights-tag: unknown argument: $1" >&2; usage ;;
  esac
done

LOG_PATH="$(corrections_log_path)"
WATERMARK_PATH="${LOG_PATH}.wm"

if [[ ! -f "$LOG_PATH" && "$DRY_RUN" != "1" ]]; then
  echo "corrections-insights-tag: $LOG_PATH does not exist; run corrections-log-init.sh first" >&2
  exit 0  # graceful: loop not installed, nothing to do
fi

if [[ -z "$INSIGHTS_FILE_ARG" ]]; then
  echo "corrections-insights-tag: --insights-file <report> is required; /fno:intel passes its report here" >&2
  exit 2
fi
if [[ ! -e "$INSIGHTS_FILE_ARG" ]]; then
  echo "corrections-insights-tag: --insights-file does not exist: $INSIGHTS_FILE_ARG" >&2
  exit 1
fi

# Watermark format: one md5 hash per line. macOS bash 3.2 doesn't have
# associative arrays, so we grep the watermark file for each candidate hash.
# Small file (one hash per insight ever ingested) so linear scan is fine.

hash_of() {
  printf '%s' "$1" | md5 -q 2>/dev/null || printf '%s' "$1" | md5sum | awk '{print $1}'
}

hash_seen() {
  [[ -f "$WATERMARK_PATH" ]] || return 1
  grep -Fxq "$1" "$WATERMARK_PATH" 2>/dev/null
}

NEW_HASHES=()
EMITTED=0
file="$INSIGHTS_FILE_ARG"
# grep -n returns "lineno:content" lines.
while IFS= read -r match; do
  [[ -z "$match" ]] && continue
  line_no="${match%%:*}"
  content="${match#*:}"
  # Key on the quoted correction text: reports cover overlapping 14-day
  # windows, so the same correction reappears on a different line with a new
  # repeat count. A line with no double quote keys on the whole line.
  quote="${content#*\"}"
  quote="${quote%%\"*}"
  hash="$(hash_of "$quote")"
  if hash_seen "$hash"; then
    continue  # already ingested in a prior run
  fi

  location="${file##*/}:${line_no}"
  details="$(corrections_escape_details "$content")"
  timestamp=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  line="${timestamp} | S2 | insights-tag | ${location} | ${details}"

  if [[ "$DRY_RUN" == "1" ]]; then
    printf '%s\n' "$line"
  else
    corrections_lock_append "$LOG_PATH" "$line" || {
      echo "corrections-insights-tag: lock-append failed for $location" >&2
      continue
    }
  fi
  NEW_HASHES+=("$hash")
  EMITTED=$((EMITTED + 1))
done < <(grep -nF "$TAG_PATTERN" "$file" 2>/dev/null || true)

# Update watermark unless dry-run.
if [[ "$DRY_RUN" != "1" && "${#NEW_HASHES[@]}" -gt 0 ]]; then
  for h in "${NEW_HASHES[@]}"; do
    printf '%s\n' "$h" >> "$WATERMARK_PATH"
  done
  chmod 600 "$WATERMARK_PATH" 2>/dev/null || true
fi

echo "corrections-insights-tag: emitted $EMITTED new entry(ies)" >&2
exit 0
