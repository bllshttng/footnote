#!/usr/bin/env bash
# autocorrect-review.sh - send a packet to a fresh Claude API call, capture
# the patch list to ~/.claude/proposed-patches/{review_id}.md.
#
# Reads a packet (from stdin or --packet-file) produced by autocorrect-pack.sh,
# substitutes it into the prompt template at
# skills/autocorrect/references/autocorrect-prompts.md, calls the Claude API,
# and writes the response.
#
# Refuses to call the API if any implicated_rules entry is missing full_text -
# the reviewer would be operating blind. Refuses if ANTHROPIC_API_KEY is unset.
#
# Exit codes:
#   0  - review captured, patch list written
#   1  - API failure (rate limit, auth, network); watermark NOT advanced
#   2  - validation failure (missing key, missing rule text, malformed packet)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROMPT_FILE="${AUTOCORRECT_PROMPT_FILE:-$REPO_ROOT/skills/autocorrect/references/autocorrect-prompts.md}"

CLAUDE_DIR="${CLAUDE_DIR_OVERRIDE:-$HOME/.claude}"
PATCHES_DIR="$CLAUDE_DIR/proposed-patches"

MODEL="${AUTOCORRECT_MODEL:-claude-sonnet-4-6}"
PACKET_FILE=""
SEVERITY_LABEL=""
PACKET_SOURCE="manual"
MAX_TOKENS=4096
API_URL="${ANTHROPIC_API_URL:-https://api.anthropic.com/v1/messages}"
ANTHROPIC_VERSION="2023-06-01"

usage() {
  cat >&2 <<'EOF'
Usage: autocorrect-review.sh [--packet-file <path>] [--model <id>] [--severity S0|S1|S2]

Options:
  --packet-file <path>    read packet from file (default: stdin)
  --model <id>            override model id (default claude-sonnet-4-6)
  --severity <tier>       label the review id; informational only
  --packet-source <id>    identifier for where the packet came from
                          (e.g. verifier-inline, watcher, manual)
  --max-tokens <n>        max response tokens (default 4096)

Env:
  ANTHROPIC_API_KEY       required
  AUTOCORRECT_MODEL       overrides default model
  AUTOCORRECT_PROMPT_FILE overrides the prompt template path
  ANTHROPIC_API_URL       overrides the API endpoint (for testing)
EOF
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --packet-file)   PACKET_FILE="${2:-}"; shift 2 ;;
    --model)         MODEL="${2:-}"; shift 2 ;;
    --severity)      SEVERITY_LABEL="${2:-}"; shift 2 ;;
    --packet-source) PACKET_SOURCE="${2:-}"; shift 2 ;;
    --max-tokens)    MAX_TOKENS="${2:-}"; shift 2 ;;
    -h|--help)       usage ;;
    *) echo "autocorrect-review: unknown argument: $1" >&2; usage ;;
  esac
done

if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
  echo "autocorrect-review: ANTHROPIC_API_KEY env var is required" >&2
  exit 2
fi

if [[ ! -f "$PROMPT_FILE" ]]; then
  echo "autocorrect-review: prompt template not found at $PROMPT_FILE" >&2
  exit 2
fi

if ! command -v curl >/dev/null 2>&1; then
  echo "autocorrect-review: curl is required" >&2
  exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "autocorrect-review: jq is required" >&2
  exit 2
fi

# Read packet from file or stdin.
TMPDIR_R="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_R"' EXIT
PACKET_TMP="$TMPDIR_R/packet.yaml"
if [[ -n "$PACKET_FILE" ]]; then
  if [[ ! -f "$PACKET_FILE" ]]; then
    echo "autocorrect-review: --packet-file not found: $PACKET_FILE" >&2
    exit 2
  fi
  cp "$PACKET_FILE" "$PACKET_TMP"
else
  cat > "$PACKET_TMP"
fi

if [[ ! -s "$PACKET_TMP" ]]; then
  echo "autocorrect-review: packet is empty" >&2
  exit 2
fi

# Validate: every implicated_rules entry must have a non-empty full_text.
# Missing full_text means the reviewer would be operating blind on that
# file. The previous version counted occurrences globally, which was
# fragile to rule files whose contents happened to include a YAML-shaped
# `full_text:` line (e.g. a rule documenting the packet format itself).
# Walk the YAML structurally with python's parser instead.
PACKET_PATH="$PACKET_TMP" python3 - <<'PY' >&2 || exit 2
import os, sys, re

with open(os.environ["PACKET_PATH"]) as f:
    text = f.read()

# Find the implicated_rules: block start and read until the next
# top-level key (no leading indent). Inside the block, each entry begins
# with "  - file:" at two-space indent and SHOULD be followed by a
# "    full_text:" at four-space indent within the same entry.
m = re.search(r"^implicated_rules:\s*$(.*?)(^\S|\Z)", text, re.MULTILINE | re.DOTALL)
if not m:
    # No implicated_rules block at all - nothing to validate.
    sys.exit(0)

block = m.group(1)
# Empty list literal "[]" on the same line as the heading is rendered by
# pack.sh when there are no entries; tolerate that.
if block.strip() == "[]":
    sys.exit(0)

# Split into entries by the "  - file:" marker.
entries = re.split(r"^  - file:\s*", block, flags=re.MULTILINE)[1:]
missing = []
for entry in entries:
    # Path is the first line of the entry (until newline).
    path = entry.split("\n", 1)[0].strip()
    # Look for a "    full_text:" line within the entry's indent block,
    # before the next "  - file:" marker (already partitioned by split).
    has_text = re.search(r"^    full_text:", entry, re.MULTILINE)
    if not has_text:
        missing.append(path)

if missing:
    print(
        "autocorrect-review: rule text not provided for all implicated files; "
        "reviewer would be operating blind", file=sys.stderr,
    )
    for p in missing:
        print(f"  - missing full_text: {p}", file=sys.stderr)
    sys.exit(2)
PY

# Extract prompt body between markers.
PROMPT_BODY="$TMPDIR_R/prompt.txt"
awk '/<!-- PROMPT_START -->/{found=1; next} /<!-- PROMPT_END -->/{found=0} found' "$PROMPT_FILE" > "$PROMPT_BODY"
if [[ ! -s "$PROMPT_BODY" ]]; then
  echo "autocorrect-review: failed to extract prompt body from $PROMPT_FILE" >&2
  exit 2
fi

# Substitute {PACKET} with the packet contents. Use a sentinel + python so
# pipe characters / special chars in the packet don't break sed.
FILLED_PROMPT="$TMPDIR_R/filled.txt"
PACKET_PATH="$PACKET_TMP" PROMPT_PATH="$PROMPT_BODY" python3 - <<'PY' > "$FILLED_PROMPT"
import os
with open(os.environ["PROMPT_PATH"]) as f:
    prompt = f.read()
with open(os.environ["PACKET_PATH"]) as f:
    packet = f.read()
print(prompt.replace("{PACKET}", packet))
PY

# Compute review_id (matches packet builder's scheme if possible).
REVIEW_ID=$(grep -E '^review_id: ' "$PACKET_TMP" | head -1 | sed 's/^review_id: //' || true)
if [[ -z "$REVIEW_ID" ]]; then
  REVIEW_ID="$(date -u +%Y-%m-%dT%H%M%SZ)-${SEVERITY_LABEL:-manual}"
fi

# Build JSON body via jq for safety.
REQUEST_BODY="$TMPDIR_R/request.json"
jq -n \
  --arg model "$MODEL" \
  --argjson max_tokens "$MAX_TOKENS" \
  --rawfile prompt "$FILLED_PROMPT" \
  '{model: $model, max_tokens: $max_tokens, messages: [{role: "user", content: $prompt}]}' \
  > "$REQUEST_BODY"

# Call the API.
RESPONSE_BODY="$TMPDIR_R/response.json"
HTTP_CODE=$(curl -sS -o "$RESPONSE_BODY" -w "%{http_code}" \
  -X POST "$API_URL" \
  -H "x-api-key: $ANTHROPIC_API_KEY" \
  -H "anthropic-version: $ANTHROPIC_VERSION" \
  -H "content-type: application/json" \
  -d @"$REQUEST_BODY" \
  || true)

if [[ "$HTTP_CODE" != "200" ]]; then
  echo "autocorrect-review: API call failed with HTTP $HTTP_CODE" >&2
  if [[ -s "$RESPONSE_BODY" ]]; then
    jq -r '.error.message // .' "$RESPONSE_BODY" 2>/dev/null >&2 || cat "$RESPONSE_BODY" >&2
  fi
  echo "autocorrect-review: watermark NOT advanced; rerun with same window to retry" >&2
  exit 1
fi

# Extract text content + usage stats.
PATCH_TEXT="$TMPDIR_R/patches.md"
jq -r '.content[]? | select(.type == "text") | .text' "$RESPONSE_BODY" > "$PATCH_TEXT" || {
  echo "autocorrect-review: failed to parse API response" >&2
  jq . "$RESPONSE_BODY" 2>&1 | head -20 >&2
  exit 1
}

INPUT_TOKENS=$(jq -r '.usage.input_tokens // 0' "$RESPONSE_BODY")
OUTPUT_TOKENS=$(jq -r '.usage.output_tokens // 0' "$RESPONSE_BODY")

# Persist patch list to proposed-patches/.
mkdir -p "$PATCHES_DIR"
OUTPUT_PATH="$PATCHES_DIR/${REVIEW_ID}.md"
{
  echo "# Autocorrect review: $REVIEW_ID"
  echo
  echo "- generated_at: $(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  echo "- model: $MODEL"
  echo "- packet_source: $PACKET_SOURCE"
  echo "- input_tokens: $INPUT_TOKENS"
  echo "- output_tokens: $OUTPUT_TOKENS"
  echo
  echo "---"
  echo
  cat "$PATCH_TEXT"
} > "$OUTPUT_PATH"
chmod 600 "$OUTPUT_PATH" 2>/dev/null || true

# Count patches: simple heuristic - look for "N. Source:" headings.
PATCH_COUNT=$(grep -cE '^[0-9]+\. Source:' "$PATCH_TEXT" || true)

echo "autocorrect-review: $PATCH_COUNT patches proposed; saved to $OUTPUT_PATH" >&2
echo "autocorrect-review: usage: ${INPUT_TOKENS} in / ${OUTPUT_TOKENS} out tokens" >&2

# Print summary to stdout (callers can pipe).
printf '%s\n' "$OUTPUT_PATH"
