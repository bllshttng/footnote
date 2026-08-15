#!/usr/bin/env bash
# scripts/ci/check-review-invocation-single-source.sh
#
# The sized code-review invocation has exactly one construction site:
# `self_review_invocation` in cli/src/fno/review_capability.py. Every other
# surface (skill prose, docs, Rust fixtures, test payloads) defers to it with
# the `<level>` placeholder. Before this check, three surfaces shipped three
# different answers (prose said `medium --fix`, the runtime verdict said a
# bare verb, the uncalled builder said `medium --comment --fix`) and the one
# test asserting the right string passed green while every caller drifted.
# A concrete level spelled next to the verb anywhere outside
# the allowlist is a fourth answer waiting to ship.
#
# `ultra` sits in the pattern on purpose: it is billed separately, so any
# autonomously copyable surface naming it fails even as "historical" prose.
#
# Allowlist:
#   cli/src/fno/review_capability.py          the builder itself
#   cli/tests/unit/test_review_capability.py  the builder's unit test (asserts
#                                             the concrete default level)
#   cli/tests/unit/fixtures/                  recorded reign history, data a
#                                             worker is never told to copy
#   this script                              it carries the canary control
#
# Both controls are load-bearing. An absence-only pass has two explanations
# ("clean" and "the instrument never matched anything"), so the tool control
# proves the pattern matches a canary, and the target control proves the tree
# still contains at least one allowlisted spelling to find. A sweep that
# returns zero raw hits has a broken pattern or a drifted allowlist and fails
# rather than passing vacuously.
#
# Exit 0 clean; 1 drift or a control that did not fire.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
cd "$REPO_ROOT"

PATTERN='(/code-review[[:space:]]+)(low|medium|high|xhigh|max|ultra)([^a-z]|$)'
ALLOW_RE='^(cli/src/fno/review_capability.py|cli/tests/unit/test_review_capability.py|cli/tests/unit/fixtures/|scripts/ci/check-review-invocation-single-source.sh)'

fail() {
    echo "check-review-invocation-single-source: $*" >&2
    exit 1
}

# Tool control: the pattern must match a canary invocation.
CANARY='/code-review medium --fix'
printf '%s\n' "$CANARY" | grep -E "$PATTERN" >/dev/null ||
    fail "pattern does not match the canary; every future sweep would pass vacuously"

# Tracked files only: CI checks out clean, and worktree-local noise never gates.
RAW="$(git grep -nE "$PATTERN" -- . || true)"
[ -n "$RAW" ] ||
    fail "zero raw hits; the allowlisted unit test must still spell the default - pattern or allowlist drifted"

BAD="$(printf '%s\n' "$RAW" | grep -Ev "$ALLOW_RE" || true)"
if [ -n "$BAD" ]; then
    echo "check-review-invocation-single-source: concrete review level(s) spelled outside the builder:" >&2
    printf '%s\n' "$BAD" >&2
    echo "Use the <level> placeholder and let self_review_invocation size it; ultra is not issuable." >&2
    exit 1
fi

COUNT="$(printf '%s\n' "$RAW" | grep -Ec "$ALLOW_RE" || true)"
echo "review invocation single-source OK: ${COUNT} allowlisted spelling(s), controls fired"
