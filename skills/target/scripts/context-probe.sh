#!/usr/bin/env bash
# context-probe.sh - Transcript-derived context window probe (model-aware).
# Self-contained skill script; no external deps beyond jq.
#
# Usage: context-probe.sh <transcript-jsonl-path>
#
# Scans the transcript for assistant messages carrying a usage block.
# Takes the LAST such line to reflect the most recent turn's context usage.
#
# Output (stdout, exit 0): one JSON line:
#   {"used_tokens": N, "window_tokens": N, "used_pct": N, "model": "..."}
#
# Exit 3 ("unreadable") when:
#   - No argument given
#   - File missing or unreadable
#   - jq not available
#   - No assistant line with a usage block exists
#   - Parsing fails
#
# The caller treats ANY nonzero exit as "no pressure" (fail-safe).
#
# Bash 3.2 compatible (macOS default). No GNU-only flags, no mapfile, no tac.

_EXIT_UNREADABLE=3

_die() {
  exit "$_EXIT_UNREADABLE"
}

# Require jq
if ! command -v jq >/dev/null 2>&1; then
  _die
fi

# Require exactly one argument
if [ $# -lt 1 ] || [ -z "$1" ]; then
  _die
fi

TRANSCRIPT="$1"

# File must exist and be readable
if [ ! -f "$TRANSCRIPT" ] || [ ! -r "$TRANSCRIPT" ]; then
  _die
fi

# Stream-filter: parse each line as JSON (skip malformed with fromjson?),
# select assistant lines that have a usage block, emit the whole object.
# Use tail -n 1 to take the LAST matching line.
# Note: fromjson? inside jq -cR skips lines that are not valid JSON.
# The pipeline may trigger SIGPIPE when tail exits early; we capture the
# exit code explicitly and ignore SIGPIPE (141).
set +o pipefail
last_line=$(jq -cR 'fromjson? | select(.type=="assistant" and (.message.usage? != null))' "$TRANSCRIPT" 2>/dev/null | tail -n 1)
jq_tail_status=$?
set -o pipefail

# If jq|tail returned an error other than SIGPIPE (141), treat as unreadable.
# SIGPIPE is acceptable (tail exited before jq finished), code 141 on bash.
if [ "$jq_tail_status" -ne 0 ] && [ "$jq_tail_status" -ne 141 ]; then
  _die
fi

# No matching line found
if [ -z "$last_line" ]; then
  _die
fi

# Extract fields from the last matching line - single jq invocation for all four fields.
# Capture output to a variable first so the jq exit code is not masked by process
# substitution; then parse via here-string (bash 3.2 compatible).
_tsv_out=""
_tsv_out="$(printf '%s' "$last_line" | jq -r \
  '[ (.message.model // ""), (.message.usage.input_tokens // 0), (.message.usage.cache_creation_input_tokens // 0), (.message.usage.cache_read_input_tokens // 0) ] | @tsv' \
  2>/dev/null)" || _die
IFS=$'\t' read -r model input_tokens cache_create cache_read <<< "$_tsv_out" || _die

# Validate they are integers (protect against null or non-numeric values)
for v in "$input_tokens" "$cache_create" "$cache_read"; do
  case "$v" in
    ''|*[!0-9]*) _die;;
  esac
done

used_tokens=$(( input_tokens + cache_create + cache_read ))

# Window size, by model family. The "[1m]" suffix is a zai/GLM routing marker;
# no Anthropic model id carries it, so matching on it ALONE put every Claude
# model on the 200K branch - a flat 5x inflation (21% real read as 108%) that
# made the caller's pressure trigger fire at a fifth of the intended usage.
# 1M is an ALLOWLIST, never a catch-all. Only ids known to have a 1M window get
# one; everything else - an older Claude, a future id, a non-Claude model - falls
# to 200K. That direction is deliberate and asymmetric: a too-small denominator
# overstates pressure and fires the handoff early, which costs one extra
# succession, while a too-large one understates it and lets the session run out
# of context, which loses the run. A `claude-*` catch-all would put every legacy
# 200K model (Opus 4.5, Sonnet 4.5) on the losing side of that trade.
# A literal table, not a lookup: it changes once per model launch.
case "$model" in
  *\[1m\]*)                              window_tokens=1000000 ;;  # zai/GLM 1M routing marker
  *haiku*)                               window_tokens=200000  ;;  # Haiku 4.5 is 200K
  *opus-5*|*sonnet-5*|*fable-5*)         window_tokens=1000000 ;;
  *opus-4-8*|*opus-4-7*|*opus-4-6*)      window_tokens=1000000 ;;
  *sonnet-4-6*)                          window_tokens=1000000 ;;
  *)                                     window_tokens=200000  ;;  # unlisted -> conservative
esac

# Integer percent, rounded: round(100 * used / window)
# Use integer arithmetic: (used * 100 + window/2) / window for round-half-up
used_pct=$(( (used_tokens * 100 + window_tokens / 2) / window_tokens ))

# Emit one JSON line on stdout
printf '{"used_tokens":%d,"window_tokens":%d,"used_pct":%d,"model":"%s"}\n' \
  "$used_tokens" "$window_tokens" "$used_pct" "$model"
