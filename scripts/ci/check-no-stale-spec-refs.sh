#!/usr/bin/env bash
# Regression gate for the /spec -> /blueprint rename sweep.
#
# The skill was renamed from /spec to /blueprint in db2d43a1 (produce -> blueprint).
# A follow-up sweep (ab-cdf21e76, 2026-05-18) cleared 117 stale /spec references
# from skills/, agents/, commands/, docs/, AGENTS.md, and CLAUDE.md. This check
# prevents the dead name from creeping back in via doc copy-paste or generated
# templates that lift stale patterns from memory / git log.
#
# Scope (matches the original sweep contract):
#   - skills/ agents/ commands/ docs/ plus AGENTS.md and CLAUDE.md at repo root
#
# Carve-outs (NOT considered stale; the rename does not apply to these):
#   - specs/ (the design-doc directory, a different concept)
#   - spec-review, gemini-spec-review, gemini-code-assist (review-tool names)
#   - specific, special, spectrum, specification (English words)
#   - .spec. file-extension form (e.g. users.spec.ts)
#   - api spec, OpenAPI spec (domain terms for API contracts)
#   - spec-template.md (the ship-docs technical-specification template)
#
# Carve-outs are enforced at the TOKEN level by the regex itself, not by a
# line-level filter. The regex requires `/spec` to terminate at one of:
# whitespace, comma, period, question mark, closing paren, single/double
# quote, exclamation, end-of-line, or be wrapped in backticks. Every term
# in the carve-out list ends with a non-terminator (a letter, dash, or
# slash that breaks the match), so the regex correctly skips them without
# needing a separate filter pass. A previous draft of this script DID use
# a line-level filter, which had a false-negative class: a stale `/spec`
# reference on the same line as a carve-out word (e.g. "/spec ... special
# flag") was incorrectly dropped. Codex P2 on PR #282 caught it.
#
# Historical artifacts that legitimately contain /spec (CHANGELOG, memory files,
# internal/ plan docs, git log output) are out of scan scope by construction.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$REPO_ROOT"

SCAN_PATHS=(skills agents commands docs AGENTS.md CLAUDE.md)

# Match the literal slash-command shape: backtick-wrapped `/spec` OR /spec
# followed by whitespace, end-of-line, comma, period, question mark, paren,
# or quote. The shape MUST stop right after `spec` so we don't pick up
# /specs/, /specific, /specification, /speculate, etc.
PATTERN='(`/spec`)|(/spec[[:space:],.?\)"'\''!]|/spec$)'

set +e
raw=$(grep -IrEn "$PATTERN" "${SCAN_PATHS[@]}" 2>/dev/null)
rc=$?
set -e
if [[ $rc -gt 1 ]]; then
  echo "AUDIT ERROR: grep failed with rc=$rc on /spec scan" >&2
  echo "  pattern: $PATTERN" >&2
  echo "  scan_paths: ${SCAN_PATHS[*]}" >&2
  exit 2
fi

if [[ -z "$raw" ]]; then
  echo "AUDIT PASS: no stale /spec slash-command references in scope."
  exit 0
fi

count=$(printf '%s' "$raw" | grep -c '^' || true)
echo "AUDIT FAIL: $count stale /spec reference(s) in:"
printf '%s\n' "$raw" | sed 's/^/  /'
echo ""
echo "Fix: rename /spec -> /blueprint in each file:line above."
echo "Background: skill was renamed in db2d43a1; carry the rename forward."
exit 1
