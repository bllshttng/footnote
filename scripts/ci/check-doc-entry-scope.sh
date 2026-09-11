#!/usr/bin/env bash
# check-doc-entry-scope.sh - CI gate over the AGENTS.md `## Deep-dive docs`
# index (x-3de3). The indexed pages describe mechanisms accurately and still
# fail a reader who arrives with a symptom: none said who the page is for or
# what the mechanism does NOT govern. Every indexed page must therefore open
# with a scope section, `## Is this page for you?`, carrying a `Not for:` line
# that names the nearby thing the page does not answer and where to look
# instead.
#
# Run: bash scripts/ci/check-doc-entry-scope.sh [agents-md-path]
# Default target AGENTS.md. Exits 0 clean; exits 1 with a report otherwise.

set -euo pipefail

TARGET="${1:-AGENTS.md}"

[[ -f "$TARGET" ]] || { echo "check-doc-entry-scope: target not found: $TARGET" >&2; exit 1; }

BASE_DIR="$(cd "$(dirname "$TARGET")" && pwd)"

# The index sweep the plan pins: everything `## Deep-dive docs` links under
# docs/, parens stripped. sort -u so a duplicated link reports once.
# grep exits 1 on zero matches; under pipefail that would kill the script
# before the zero-link refusal below can speak, so let the pipeline fail soft
# and let the empty-string check decide.
LINKS=$(awk '/^## Deep-dive docs/,0' "$TARGET" | grep -o '(docs/[^)]*\.md)' | tr -d '()' | sort -u || true)

# A zero-link parse cannot pass: the gate refuses rather than checks nothing.
if [[ -z "$LINKS" ]]; then
  {
    echo "check-doc-entry-scope: the '## Deep-dive docs' section in ${TARGET} yielded zero docs/ links."
    echo "  Either the section is gone or its links no longer match '(docs/*.md)'."
  } >&2
  exit 1
fi

VIOLATIONS=0
REPORT=""
PAGES=0
add_violation() {
  REPORT+="[doc-scope] $1"$'\n'
  VIOLATIONS=$((VIOLATIONS + 1))
}

while IFS= read -r rel; do
  [[ -z "$rel" ]] && continue
  PAGE="${BASE_DIR}/${rel}"
  if [[ ! -f "$PAGE" ]]; then
    add_violation "'${rel}' is linked from the Deep-dive docs index but the file is missing"
    continue
  fi
  PAGES=$((PAGES + 1))
  FIRST_H2=$(awk '/^## /{ print; exit }' "$PAGE")
  if [[ "$FIRST_H2" != "## Is this page for you?" ]]; then
    add_violation "'${rel}' must open with '## Is this page for you?' as its first '## ' heading (found: ${FIRST_H2:-none})"
    continue
  fi
  # Scope-section body: lines after the first ^## heading, up to the next one.
  SECTION=$(awk '/^## /{ if (seen) exit; seen = 1; next } seen { print }' "$PAGE")
  if ! grep -q '^Not for:' <<< "$SECTION"; then
    add_violation "'${rel}' scope section has no 'Not for:' line naming what the page does not govern"
  fi
done <<< "$LINKS"

if [[ $VIOLATIONS -eq 0 ]]; then
  echo "check-doc-entry-scope: ${PAGES} pages checked"
  exit 0
fi

{
  echo "check-doc-entry-scope: ${VIOLATIONS} violation(s)"
  echo
  printf '%s' "$REPORT"
  echo
  echo "Fix: give each page a scope section as its first '## ' heading:"
  echo "  '## Is this page for you?', with stakes, a 'Not for:' line naming"
  echo "  the neighboring mechanism the page does not govern and linking"
  echo "  where that reader should go instead."
} >&2
exit 1
