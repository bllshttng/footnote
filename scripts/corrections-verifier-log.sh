#!/usr/bin/env bash
# corrections-verifier-log.sh - wrapper any pre-commit verifier can invoke
# to record a correction event.
#
# Designed to be called by verifier scripts in production repos (your project,
# abilities, etc.) when they block a commit. Appends exactly one structured
# line to ~/.claude/corrections.log with locking.
#
# Usage:
#   corrections-verifier-log.sh \
#     --source emdash-grep \
#     --location src/blog/post.md:42 \
#     --severity S1 \
#     --details "unicode emdash detected in agent-authored markdown"
#
# Exit codes:
#   0 - line written
#   1 - lock contention / write failure
#   2 - validation failure (missing flag, unknown severity)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
source "$SCRIPT_DIR/lib/corrections-lock.sh"

SOURCE=""
LOCATION="-"
SEVERITY=""
DETAILS=""

usage() {
  cat >&2 <<'EOF'
Usage: corrections-verifier-log.sh --source <id> --severity <S0|S1|S2> [--location <where>] [--details <text>]

Required:
  --source     short writer identifier (e.g. emdash-grep, secret-scanner)
  --severity   one of S0, S1, S2 (closed enum)

Optional:
  --location   file:line or session-id or repo name (default "-")
  --details    free-text, max 200 chars, pipes auto-escaped
EOF
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --source)   SOURCE="${2:-}";    shift 2 ;;
    --location) LOCATION="${2:-}";  shift 2 ;;
    --severity) SEVERITY="${2:-}";  shift 2 ;;
    --details)  DETAILS="${2:-}";   shift 2 ;;
    -h|--help)  usage ;;
    *) echo "corrections-verifier-log: unknown argument: $1" >&2; usage ;;
  esac
done

if [[ -z "$SOURCE" ]]; then
  echo "corrections-verifier-log: --source is required" >&2
  exit 2
fi
if [[ "$SOURCE" == *"|"* ]]; then
  echo "corrections-verifier-log: --source cannot contain '|' character" >&2
  exit 2
fi
if [[ -z "$SEVERITY" ]]; then
  echo "corrections-verifier-log: --severity is required" >&2
  exit 2
fi
if ! corrections_validate_severity "$SEVERITY"; then
  exit 2
fi

LOG_PATH="$(corrections_log_path)"
if [[ ! -f "$LOG_PATH" ]]; then
  # Don't bootstrap from a verifier path - that's the install script's job.
  # Surface to stderr so the caller knows the loop is not active.
  echo "corrections-verifier-log: $LOG_PATH does not exist; run scripts/corrections-log-init.sh first" >&2
  exit 1
fi

# Default location must not be empty after parsing.
[[ -z "$LOCATION" ]] && LOCATION="-"

# Escape DETAILS (pipes, newlines, length cap).
SAFE_DETAILS="$(corrections_escape_details "$DETAILS")"
TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

LINE="${TIMESTAMP} | ${SEVERITY} | ${SOURCE} | ${LOCATION} | ${SAFE_DETAILS}"

corrections_lock_append "$LOG_PATH" "$LINE"

# S0 inline trigger: if the reviewer is installed, fire a same-day review.
# Falls through silently if autocorrect-review.sh / autocorrect-pack.sh
# aren't installed yet (Phase 01-only deploys still get the capture path).
# Use AUTOCORRECT_DISABLE_INLINE_S0=1 to suppress (e.g., in test runs).
#
# S0 is high-severity by definition. We pass the reviewer's stderr through
# (only stdout is suppressed) so a failed inline trigger surfaces the
# real reason in the terminal that triggered the commit. The watcher will
# also retry within 15 minutes, but the user should see why the inline
# path failed without waiting.
if [[ "$SEVERITY" == "S0" && "${AUTOCORRECT_DISABLE_INLINE_S0:-0}" != "1" ]]; then
  PACK_SCRIPT="$SCRIPT_DIR/autocorrect-pack.sh"
  REVIEW_SCRIPT="$SCRIPT_DIR/autocorrect-review.sh"
  if [[ -x "$PACK_SCRIPT" && -x "$REVIEW_SCRIPT" && -n "${ANTHROPIC_API_KEY:-}" ]]; then
    if PACKET=$("$PACK_SCRIPT" --severity S0 --dry-run); then
      printf '%s\n' "$PACKET" \
        | "$REVIEW_SCRIPT" --severity S0 --packet-source verifier-inline \
        >/dev/null \
        || echo "corrections-verifier-log: S0 inline review failed; watcher will retry" >&2
    else
      echo "corrections-verifier-log: S0 packet build failed; watcher will retry" >&2
    fi
  fi
fi
