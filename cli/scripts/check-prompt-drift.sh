#!/usr/bin/env bash
# check-prompt-drift.sh
#
# Compare bundled CLI agent prompts against the canonical plugin agent files.
# Exits 0 if every prompt body matches. Exits 1 if any prompt has drifted
# from its plugin counterpart, or if a bundled prompt has no matching agent.
#
# Usage:
#   bash cli/scripts/check-prompt-drift.sh [--root <path>]
#
# --root <path>   Repo root to use (default: git rev-parse --show-toplevel).
#                 The test suite passes a temp directory via this flag.
#
# Allowed annotation on the CLI side only:
#   <!-- cli-context-prefix -->
#   ...CLI-specific boilerplate...
#   <!-- /cli-context-prefix -->
# This block is stripped from the CLI prompt before comparison so legitimate
# CLI context additions do not count as drift.
#
# Exit codes:
#   0  - all prompts match their plugin agents
#   1  - one or more prompts drifted or are orphans
set -euo pipefail

# ---- Argument parsing ----
ROOT=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)
      ROOT="$2"
      shift 2
      ;;
    *)
      echo "ERROR: unknown argument $1" >&2
      exit 1
      ;;
  esac
done

# Resolve repo root
if [[ -z "$ROOT" ]]; then
  ROOT=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
fi

PROMPTS_DIR="${ROOT}/cli/src/fno/review/prompts"
AGENTS_DIR="${ROOT}/agents"

# ---- Helpers ----

# strip_frontmatter: read from stdin, emit body (after second --- delimiter).
# Compatible with bash 3.2 (macOS) - uses only POSIX awk.
strip_frontmatter() {
  awk '
    BEGIN { delim_count=0; past_fm=0 }
    /^---[[:space:]]*$/ {
      delim_count++
      if (delim_count == 2) { past_fm=1 }
      next
    }
    past_fm { print }
  '
}

# strip_cli_prefix: remove <!-- cli-context-prefix --> ... <!-- /cli-context-prefix --> block.
# Reads from stdin. The closing tag must appear on its own line.
strip_cli_prefix() {
  awk '
    /<!-- cli-context-prefix -->/ { in_block=1; next }
    /<!-- \/cli-context-prefix -->/ { in_block=0; next }
    !in_block { print }
  '
}

# strip_trailing_whitespace: remove trailing spaces/tabs from each line.
# Uses sed for portability across macOS (BSD sed) and Linux (GNU sed).
strip_trailing_whitespace() {
  sed 's/[[:space:]]*$//'
}

# ---- Main ----

if [[ ! -d "$PROMPTS_DIR" ]]; then
  # No prompts directory = nothing to check = clean.
  echo "INFO: No prompts directory found at ${PROMPTS_DIR}, nothing to check."
  exit 0
fi

exit_code=0

for cli_file in "${PROMPTS_DIR}"/*.md; do
  # If glob matched nothing, the literal pattern is returned.
  if [[ ! -f "$cli_file" ]]; then
    break
  fi

  cli_basename=$(basename "$cli_file" .md)
  # Convert underscores to hyphens to find the matching plugin agent.
  agent_name=$(echo "$cli_basename" | sed 's/_/-/g')
  agent_file="${AGENTS_DIR}/${agent_name}.md"

  if [[ ! -f "$agent_file" ]]; then
    echo "ORPHAN: ${cli_file} has no matching plugin agent (expected ${agent_file})"
    exit_code=1
    continue
  fi

  # Extract body from each side, stripping trailing whitespace for comparison.
  # Command substitution strips trailing newlines - use temp files to preserve content.
  tmp_cli=$(mktemp /tmp/drift-cli.XXXXXX)
  tmp_agent=$(mktemp /tmp/drift-agent.XXXXXX)
  # Clean up temps on script exit.
  trap 'rm -f "$tmp_cli" "$tmp_agent"' EXIT

  strip_frontmatter < "$cli_file" | strip_cli_prefix | strip_trailing_whitespace > "$tmp_cli"
  strip_frontmatter < "$agent_file" | strip_trailing_whitespace > "$tmp_agent"

  if ! diff -u "$tmp_agent" "$tmp_cli" > /dev/null 2>&1; then
    echo "DRIFT: ${cli_file} diverges from ${agent_file}"
    diff -u "$tmp_agent" "$tmp_cli" || true
    echo ""
    exit_code=1
  fi

  rm -f "$tmp_cli" "$tmp_agent"
  trap - EXIT
done

exit "$exit_code"
