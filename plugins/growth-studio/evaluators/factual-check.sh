#!/usr/bin/env bash
# factual-check.sh <draft> <product-truth-path>
#
# A deterministic STRUCTURAL check, never a semantic judgment. Fails when the
# draft has no '## Claims' section, or when any claim bullet lacks a citation
# that resolves to a heading in the supplied product-truth file. The
# product-truth path is the second argument, never hardcoded, so the same check
# runs against whatever the installing project configured.
#
# Citation convention: a claim bullet cites a product-truth heading with a
# bracket reference whose text matches the heading exactly, e.g.
#   - Footnote ships a delivery graph [Delivery graph].
# matches a '## Delivery graph' heading in product-truth.
#
# POSIX-portable: no mapfile (bash 3.2), no grep -P (BSD grep).
set -euo pipefail

draft="${1:?usage: factual-check.sh <draft> <product-truth-path>}"
truth="${2:?usage: factual-check.sh <draft> <product-truth-path>}"

[[ -f "$draft" ]] || { echo "factual-check: draft not found: $draft" >&2; exit 2; }
[[ -f "$truth" ]] || { echo "factual-check: product-truth not found: $truth" >&2; exit 2; }

# 1. A '## Claims' section must exist (distinct from an empty one).
if ! grep -qE '^##[[:space:]]+Claims([[:space:]]|$)' "$draft"; then
  echo "factual-check: draft has no '## Claims' section" >&2
  exit 1
fi

# 2. Claim bullets: the '- ' lines that follow '## Claims' until the next heading.
claims=()
while IFS= read -r line; do claims+=("$line"); done < <(awk '
  /^##[[:space:]]+Claims([[:space:]]|$)/ { in_claims = 1; next }
  in_claims && /^##[[:space:]]/ { in_claims = 0 }
  in_claims && /^[[:space:]]*-[[:space:]]/ { sub(/^[[:space:]]*-[[:space:]]*/, ""); print }
' "$draft")

if [ "${#claims[@]}" -eq 0 ]; then
  echo "factual-check: '## Claims' section has no claim bullets" >&2
  exit 1
fi

# 3. Heading titles in the product-truth file (text after one or more '#').
headings_file="$(mktemp)"
trap 'rm -f "$headings_file"' EXIT
grep -E '^##[[:space:]]+' "$truth" | sed -E 's/^##+[[:space:]]+//; s/[[:space:]]+$//' > "$headings_file"

fail=0
for claim in "${claims[@]}"; do
  # Bracket citation tokens in the bullet.
  cites=()
  while IFS= read -r tok; do [ -n "$tok" ] && cites+=("$tok"); done < <(
    printf '%s' "$claim" | grep -oE '\[[^]]+\]' | sed -E 's/^\[//; s/\]$//'
  )
  if [ "${#cites[@]}" -eq 0 ]; then
    echo "factual-check: claim lacks a citation: ${claim:0:80}" >&2
    fail=1
    continue
  fi
  for cite in "${cites[@]}"; do
    if ! grep -qxF -- "$cite" "$headings_file"; then
      echo "factual-check: claim cites a heading absent from product-truth: [$cite]" >&2
      fail=1
    fi
  done
done

# Citations may appear in the body too, not only under Claims. Every bracket
# citation in the WHOLE draft must resolve to a product-truth heading, except
# markdown links [text](url) (a `(` immediately follows). This stops an
# unsupported assertion anywhere from passing just because it sits outside the
# Claims section.
all_cites=()
while IFS= read -r tok; do [ -n "$tok" ] && all_cites+=("$tok"); done < <(
  # Strip markdown links first ([text](url) -> ""), then collect [Heading] tokens.
  sed -E 's/\[[^]]*\]\([^)]*\)//g' "$draft" \
    | grep -oE '\[[^]]+\]' | sed -E 's/^\[//; s/\]$//'
)
if [ "${#all_cites[@]}" -gt 0 ]; then
  for cite in "${all_cites[@]}"; do
    if ! grep -qxF -- "$cite" "$headings_file"; then
      echo "factual-check: citation does not resolve to a product-truth heading: [$cite]" >&2
      fail=1
    fi
  done
fi

exit $fail
