#!/usr/bin/env bash
# corrections-insights-tag.sh - ingest /insights entries tagged #agent-correction
# into corrections.log as S2 events.
#
# This is the only user-mediated capture path: the user tags an /insights entry
# (or asks Claude to add the tag) when reviewing /insights output. This script
# ports the tagged entries into corrections.log.
#
# Resolution order for the insights source:
#   1. --insights-file <path>  (explicit override)
#   2. $INSIGHTS_FILE env var
#   3. ~/.claude/insights.md (single-file convention)
#   4. ~/.claude/insights/*.md (per-session convention)
#
# Tracks a watermark via a content hash so re-runs do not double-ingest.
# Watermark file: ~/.claude/.insights-watermark (line-delimited, one hash per line).
#
# Exit 0 when /insights is not available (graceful no-op). Exit non-zero only
# on actual errors (lock failure, malformed flag, etc).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
source "$SCRIPT_DIR/lib/corrections-lock.sh"

INSIGHTS_FILE_ARG=""
DRY_RUN=0
TAG_PATTERN="#agent-correction"

usage() {
  cat >&2 <<'EOF'
Usage: corrections-insights-tag.sh [--insights-file <path>] [--dry-run]

Reads /insights output and emits S2 corrections.log entries for every line
containing #agent-correction. Watermark prevents re-ingestion of already-seen
entries.

Options:
  --insights-file <path>   override default discovery
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

CLAUDE_DIR="${CLAUDE_DIR_OVERRIDE:-$HOME/.claude}"
LOG_PATH="$(corrections_log_path)"
WATERMARK_PATH="$CLAUDE_DIR/.insights-watermark"

if [[ ! -f "$LOG_PATH" && "$DRY_RUN" != "1" ]]; then
  echo "corrections-insights-tag: $LOG_PATH does not exist; run corrections-log-init.sh first" >&2
  exit 0  # graceful: loop not installed, nothing to do
fi

# Discover the insights source.
INSIGHTS_FILES=()
if [[ -n "$INSIGHTS_FILE_ARG" ]]; then
  if [[ ! -e "$INSIGHTS_FILE_ARG" ]]; then
    echo "corrections-insights-tag: --insights-file does not exist: $INSIGHTS_FILE_ARG" >&2
    exit 1
  fi
  INSIGHTS_FILES+=("$INSIGHTS_FILE_ARG")
elif [[ -n "${INSIGHTS_FILE:-}" && -e "$INSIGHTS_FILE" ]]; then
  INSIGHTS_FILES+=("$INSIGHTS_FILE")
elif [[ -f "$CLAUDE_DIR/insights.md" ]]; then
  INSIGHTS_FILES+=("$CLAUDE_DIR/insights.md")
elif [[ -d "$CLAUDE_DIR/insights" ]]; then
  while IFS= read -r -d '' f; do
    INSIGHTS_FILES+=("$f")
  done < <(find "$CLAUDE_DIR/insights" -maxdepth 1 -type f -name "*.md" -print0 2>/dev/null)
fi

if [[ "${#INSIGHTS_FILES[@]}" -eq 0 ]]; then
  echo "corrections-insights-tag: no /insights source found; this is a no-op" >&2
  exit 0
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
for file in "${INSIGHTS_FILES[@]}"; do
  # grep -n returns "lineno:content" lines.
  while IFS= read -r match; do
    [[ -z "$match" ]] && continue
    line_no="${match%%:*}"
    content="${match#*:}"
    entry_key="${file}:${line_no}:${content}"
    hash="$(hash_of "$entry_key")"
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
        echo "corrections-insights-tag: lock-append failed for $entry_key" >&2
        continue
      }
    fi
    NEW_HASHES+=("$hash")
    EMITTED=$((EMITTED + 1))
  done < <(grep -nF "$TAG_PATTERN" "$file" 2>/dev/null || true)
done

# Update watermark unless dry-run.
if [[ "$DRY_RUN" != "1" && "${#NEW_HASHES[@]}" -gt 0 ]]; then
  for h in "${NEW_HASHES[@]}"; do
    printf '%s\n' "$h" >> "$WATERMARK_PATH"
  done
  chmod 600 "$WATERMARK_PATH" 2>/dev/null || true
fi

echo "corrections-insights-tag: emitted $EMITTED new entry(ies)" >&2
exit 0
