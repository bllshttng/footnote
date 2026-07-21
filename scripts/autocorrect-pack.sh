#!/usr/bin/env bash
# autocorrect-pack.sh - assemble a review packet for the autocorrect loop.
#
# Reads corrections.log, ~/.fno/graph.json (BLOCKED state mirror), and
# the git log of ~/.claude/, and produces a yaml document containing events
# within the time window plus the current full text of every rule file
# referenced by those events.
#
# The packet is fed to autocorrect-review.sh (Task 2.2), which sends it to a
# fresh Claude API call.
#
# Defaults:
#   window: 30 days (or since watermark, whichever is more recent)
#   severity filter: S1,S2 (monthly review). Pass --severity S0 to filter to S0 only.
#   format: yaml

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
source "$SCRIPT_DIR/lib/corrections-lock.sh"

CLAUDE_DIR="${CLAUDE_DIR_OVERRIDE:-$HOME/.claude}"
LOG_PATH="$(corrections_log_path)"
WATERMARK_PATH="$CLAUDE_DIR/.corrections-watermark"
GRAPH_PATH="${FNO_GRAPH_PATH:-$HOME/.fno/graph.json}"

WINDOW_DAYS=30
SEVERITY_FILTER="S1,S2"
DRY_RUN=0
SIZE_WARN_KB=100

usage() {
  cat >&2 <<'EOF'
Usage: autocorrect-pack.sh [--window <Nd>] [--severity <list>] [--dry-run]

Options:
  --window 30d        time window for events (default 30d)
  --severity S1,S2    comma-separated severity filter (default S1,S2; use S0 for immediate)
  --dry-run           write to stdout only; do not advance watermark

Output: yaml document on stdout. Stderr carries diagnostics.
EOF
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --window) WINDOW_DAYS="${2%d}"; shift 2 ;;
    --severity) SEVERITY_FILTER="${2:-}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage ;;
    *) echo "autocorrect-pack: unknown argument: $1" >&2; usage ;;
  esac
done

if [[ ! -f "$LOG_PATH" ]]; then
  echo "autocorrect-pack: $LOG_PATH does not exist" >&2
  exit 1
fi

# Compute window bounds. WINDOW_DAYS is days back from now.
# If watermark exists and is more recent than WINDOW_DAYS ago, use watermark instead.
NOW_EPOCH=$(date -u +%s)
WINDOW_START_EPOCH=$((NOW_EPOCH - WINDOW_DAYS * 86400))
if [[ -f "$WATERMARK_PATH" ]]; then
  WM_VAL=$(head -1 "$WATERMARK_PATH" 2>/dev/null || echo "")
  if [[ -n "$WM_VAL" ]]; then
    # Convert ISO timestamp to epoch. A failure here is a real problem
    # (watermark corrupted) so we surface it to stderr rather than
    # silently falling back to the 30-day window.
    WM_EPOCH=""
    if [[ "$(uname)" == "Darwin" ]]; then
      WM_EPOCH=$(date -j -u -f "%Y-%m-%dT%H:%M:%SZ" "$WM_VAL" +%s 2>/dev/null || echo "")
    else
      WM_EPOCH=$(date -u -d "$WM_VAL" +%s 2>/dev/null || echo "")
    fi
    if [[ -z "$WM_EPOCH" ]]; then
      echo "autocorrect-pack: watermark at $WATERMARK_PATH is unparseable: '$WM_VAL'" >&2
      echo "autocorrect-pack: falling back to $WINDOW_DAYS-day window" >&2
    elif [[ "$WM_EPOCH" -gt "$WINDOW_START_EPOCH" ]]; then
      WINDOW_START_EPOCH="$WM_EPOCH"
    fi
  fi
fi

iso_from_epoch() {
  local e="$1"
  if [[ "$(uname)" == "Darwin" ]]; then
    date -u -r "$e" +"%Y-%m-%dT%H:%M:%SZ"
  else
    date -u -d "@$e" +"%Y-%m-%dT%H:%M:%SZ"
  fi
}

WINDOW_START_ISO="$(iso_from_epoch "$WINDOW_START_EPOCH")"
WINDOW_END_ISO="$(iso_from_epoch "$NOW_EPOCH")"
REVIEW_ID="$(date -u +%Y-%m-%d)-$(printf "%s" "$SEVERITY_FILTER" | tr ',' '-')"

# Build severity filter pattern (egrep alternation).
SEVERITY_REGEX="$(printf "%s" "$SEVERITY_FILTER" | sed 's/,/|/g')"

# -------------------------------------------------------------------
# Filter corrections.log by window and severity.
# Each line: TIMESTAMP | SEVERITY | SOURCE | LOCATION | DETAILS
# -------------------------------------------------------------------
TMPDIR_PACK="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_PACK"' EXIT
FILTERED_EVENTS="$TMPDIR_PACK/events.txt"
: > "$FILTERED_EVENTS"

while IFS= read -r line; do
  [[ -z "$line" ]] && continue
  # Split on " | ". awk handles malformed lines gracefully.
  ts="$(printf '%s' "$line" | awk -F' \\| ' '{print $1}')"
  sev="$(printf '%s' "$line" | awk -F' \\| ' '{print $2}')"
  [[ -z "$ts" || -z "$sev" ]] && continue
  if ! printf "%s" "$sev" | grep -qE "^($SEVERITY_REGEX)$"; then
    continue
  fi
  # Compare timestamp to window. A malformed timestamp is logged and
  # skipped rather than silently coerced to epoch 0 (which would drop
  # the event from the packet without signal to the user).
  ts_epoch=""
  if [[ "$(uname)" == "Darwin" ]]; then
    ts_epoch=$(date -j -u -f "%Y-%m-%dT%H:%M:%SZ" "$ts" +%s 2>/dev/null || echo "")
  else
    ts_epoch=$(date -u -d "$ts" +%s 2>/dev/null || echo "")
  fi
  if [[ -z "$ts_epoch" ]]; then
    echo "autocorrect-pack: skipping line with malformed timestamp '$ts'" >&2
    continue
  fi
  if [[ "$ts_epoch" -ge "$WINDOW_START_EPOCH" && "$ts_epoch" -le "$NOW_EPOCH" ]]; then
    printf '%s\n' "$line" >> "$FILTERED_EVENTS"
  fi
done < "$LOG_PATH"

EVENT_COUNT=$(wc -l < "$FILTERED_EVENTS" | tr -d ' ')

# -------------------------------------------------------------------
# Collect implicated files (from LOCATION field). Resolve full text or
# mark as deleted.
# -------------------------------------------------------------------
IMPLICATED_LIST="$TMPDIR_PACK/implicated.txt"
: > "$IMPLICATED_LIST"
while IFS= read -r line; do
  loc="$(printf '%s' "$line" | awk -F' \\| ' '{print $4}')"
  [[ -z "$loc" || "$loc" == "-" ]] && continue
  # Strip :line suffix if present.
  file_path="${loc%%:*}"
  # Expand ~ if present.
  file_path="${file_path/#\~/$HOME}"
  # Only treat as a file reference if it looks like a path (contains /
  # or .) or actually exists. Session-ids and repo-names without
  # separators don't pollute implicated_rules.
  if [[ "$file_path" != */* && "$file_path" != *.* && ! -f "$file_path" ]]; then
    continue
  fi
  printf '%s\n' "$file_path" >> "$IMPLICATED_LIST"
done < "$FILTERED_EVENTS"
UNIQ_IMPLICATED="$TMPDIR_PACK/implicated-uniq.txt"
sort -u "$IMPLICATED_LIST" > "$UNIQ_IMPLICATED"

# -------------------------------------------------------------------
# graph.json BLOCKED state, if available.
# -------------------------------------------------------------------
GRAPH_BLOCKED="$TMPDIR_PACK/graph-blocked.yaml"
: > "$GRAPH_BLOCKED"
if [[ -f "$GRAPH_PATH" ]] && command -v jq >/dev/null 2>&1; then
  # Select nodes where status == "blocked" OR blocked_count > 0.
  jq -r '
    .nodes // [] | .[] |
    select(.status == "blocked" or (.blocked_count // 0) > 0) |
    {
      node_id: .id,
      title: .title,
      last_blocked_reason: (.last_blocked_reason // null),
      blocked_count: (.blocked_count // 0)
    } |
    "  - node_id: " + .node_id + "\n" +
    "    title: " + (.title | tojson) + "\n" +
    "    blocked_count: " + (.blocked_count | tostring) + "\n" +
    "    last_blocked_reason: " + (.last_blocked_reason | tojson)
  ' "$GRAPH_PATH" 2>/dev/null >> "$GRAPH_BLOCKED" || true
fi

# -------------------------------------------------------------------
# Emit yaml.
# -------------------------------------------------------------------
emit() {
  printf '%s\n' "$1"
}

OUTPUT="$TMPDIR_PACK/packet.yaml"
{
  emit "review_id: $REVIEW_ID"
  emit "window_start: $WINDOW_START_ISO"
  emit "window_end: $WINDOW_END_ISO"
  emit "severity_filter: [$(printf "%s" "$SEVERITY_FILTER" | sed 's/,/, /g')]"
  emit "event_count: $EVENT_COUNT"
  emit ""
  emit "events:"
  if [[ "$EVENT_COUNT" -gt 0 ]]; then
    while IFS= read -r line; do
      ts="$(printf '%s' "$line" | awk -F' \\| ' '{print $1}')"
      sev="$(printf '%s' "$line" | awk -F' \\| ' '{print $2}')"
      src="$(printf '%s' "$line" | awk -F' \\| ' '{print $3}')"
      loc="$(printf '%s' "$line" | awk -F' \\| ' '{print $4}')"
      det="$(printf '%s' "$line" | awk -F' \\| ' '{print $5}')"
      # Unescape pipes for yaml output.
      det="${det//\\|/|}"
      emit "  - timestamp: $ts"
      emit "    severity: $sev"
      emit "    source: $src"
      emit "    location: \"$loc\""
      # yaml escape: wrap in double quotes, escape internal quotes.
      esc_det="${det//\"/\\\"}"
      emit "    details: \"$esc_det\""
    done < "$FILTERED_EVENTS"
  else
    emit "  []"
  fi
  emit ""
  emit "graph_blocked_state:"
  if [[ -s "$GRAPH_BLOCKED" ]]; then
    cat "$GRAPH_BLOCKED"
  else
    emit "  []"
  fi
  emit ""
  emit "git_log_claude_dir: |"
  if [[ -d "$CLAUDE_DIR/.git" ]]; then
    ( cd "$CLAUDE_DIR" && git log --since="$WINDOW_START_ISO" --oneline 2>/dev/null | sed 's/^/  /' ) || true
  else
    emit "  (no git history; ~/.claude is not a git repo)"
  fi
  emit ""
  emit "implicated_rules:"
  if [[ -s "$UNIQ_IMPLICATED" ]]; then
    while IFS= read -r file_path; do
      emit "  - file: $file_path"
      if [[ -f "$file_path" ]]; then
        # YAML literal block with 6-space indent (2 for list item + 4 for nested key body)
        emit "    full_text: |"
        line_count=$(wc -l < "$file_path" | tr -d ' ')
        if [[ "$line_count" -gt 200 ]]; then
          head -200 "$file_path" | sed 's/^/      /'
          emit "      [... truncated; original is $line_count lines ...]"
        else
          sed 's/^/      /' "$file_path"
        fi
      else
        emit "    full_text: \"<file deleted or not found at packet build time>\""
      fi
    done < "$UNIQ_IMPLICATED"
  else
    emit "  []"
  fi
  emit ""
  emit "watermark:"
  emit "  last_review: $WINDOW_END_ISO"
  emit "  last_review_id: $REVIEW_ID"
} > "$OUTPUT"

# Size check.
PACKET_SIZE_KB=$(( $(wc -c < "$OUTPUT") / 1024 ))
if [[ "$PACKET_SIZE_KB" -gt "$SIZE_WARN_KB" ]]; then
  echo "autocorrect-pack: WARNING packet size ${PACKET_SIZE_KB}KB exceeds ${SIZE_WARN_KB}KB cap" >&2
  echo "autocorrect-pack: consider narrowing --window or splitting --severity S1 / S2 reviews" >&2
fi

cat "$OUTPUT"

# Update watermark unless dry-run.
if [[ "$DRY_RUN" != "1" ]]; then
  printf '%s\n' "$WINDOW_END_ISO" > "$WATERMARK_PATH"
  chmod 600 "$WATERMARK_PATH" 2>/dev/null || true
fi
