#!/usr/bin/env bash
# accessibility-check.sh <draft>
#
# A deterministic STRUCTURAL check for the design role's rendered-mock
# deliverables. Fails when a referenced image has empty alt text or the draft
# omits a contrast note. Judgment (is the contrast ratio sufficient?) stays
# with the role; this only checks the evidence is present.
set -euo pipefail

draft="${1:?usage: accessibility-check.sh <draft>}"
[[ -f "$draft" ]] || { echo "accessibility-check: draft not found: $draft" >&2; exit 2; }

fail=0

# 1. An image reference with empty alt text: ![alt](src) where alt is blank.
if grep -nE '!\[[[:space:]]*\]\(' "$draft" >/dev/null; then
  echo "accessibility-check: image with no alt text" >&2
  fail=1
fi

# 2. A rendered mock must include at least one image with NON-EMPTY alt text.
#    A draft with no rendered asset is not an accessible mock.
if ! grep -qE '!\[[^]]+\]\(' "$draft"; then
  echo "accessibility-check: no rendered image with alt text" >&2
  fail=1
fi

# 3. Structured contrast evidence: a ratio like 4.5:1 (not just the word
#    "contrast"). Keeps this a structural check, not a ratio judgment.
if ! grep -qE '[0-9]+(\.[0-9]+)?:[0-9]+|[0-9]+(\.[0-9]+)?/1\b' "$draft"; then
  echo "accessibility-check: no contrast ratio (expected e.g. 4.5:1)" >&2
  fail=1
fi

exit $fail
