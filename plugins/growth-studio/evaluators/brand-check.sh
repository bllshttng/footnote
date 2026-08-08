#!/usr/bin/env bash
# brand-check.sh <draft> [brand-identity]
#
# A deterministic STRUCTURAL voice check. Fails when the draft contains a
# banned term (read from the 'Don't' block of assets/brand-voice.md, so the
# asset is the rule source and this script is only the enforcement), an em dash
# (U+2014), or a prose paragraph that wraps one sentence across physical lines.
# When a brand-identity path is given, also fails on a founder-name form
# declared `never:` under its `## Founder` block: the per-project name rule,
# enforced rather than left to prose.
#
# POSIX-portable: no mapfile (bash 3.2), no grep -P (BSD grep). The em dash is
# matched by its UTF-8 byte sequence so no literal em dash appears in source.
set -euo pipefail

draft="${1:?usage: brand-check.sh <draft>}"
[[ -f "$draft" ]] || { echo "brand-check: draft not found: $draft" >&2; exit 2; }

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
voice="$script_dir/../assets/brand-voice.md"
[[ -f "$voice" ]] || { echo "brand-check: brand-voice asset not found: $voice" >&2; exit 2; }

identity="${2:-}"
if [[ -n "$identity" ]]; then
  [[ -f "$identity" ]] || { echo "brand-check: brand-identity not found: $identity" >&2; exit 2; }
fi

fail=0

# 1. Em dash (U+2014 = E2 80 94) anywhere in the draft.
em_dash=$'\xe2\x80\x94'
if grep -nF -- "$em_dash" "$draft" >/dev/null; then
  echo "brand-check: em dash (U+2014) present" >&2
  fail=1
fi

# 2. Banned terms from the brand-voice 'Don't' block. Each bullet's leading
#    phrase (trailing parenthetical stripped) is a banned term.
banned=()
while IFS= read -r line; do [ -n "$line" ] && banned+=("$line"); done < <(awk '
  /^##[[:space:]]+Don.?t([[:space:]]|$)/ { in_dont = 1; next }
  in_dont && /^##[[:space:]]/ { in_dont = 0 }
  in_dont && /^[[:space:]]*-[[:space:]]/ {
    line = $0
    sub(/^[[:space:]]*-[[:space:]]*/, "", line)
    sub(/[[:space:]]*\(.*$/, "", line)
    if (line != "") print line
  }
' "$voice")
if [ "${#banned[@]}" -gt 0 ]; then
  for term in "${banned[@]}"; do
    if grep -qniF -- "$term" "$draft"; then
      echo "brand-check: banned term present: $term" >&2
      fail=1
    fi
  done
fi

# 3. One sentence per physical line. A prose line that does not end in terminal
#    punctuation (. ! ? : ;) is a sentence split across lines, because a
#    complete sentence on one line always terminates. Skip headings, list
#    items, tables, code fences, image lines, and blank lines.
wrapped="$(awk '
  BEGIN { infence = 0 }
  /^[[:space:]]*$/ { next }
  /^```/ { infence = !infence; next }
  infence { next }
  /^[#]/ { next }
  /^[[:space:]]*[-*+][[:space:]]/ { next }
  /^[[:space:]]*\|/ { next }
  /^[[:space:]]*!\[/ { next }
  {
    last = substr($0, length($0), 1)
    if (last !~ /[.!?:;]/) { print NR }
  }
' "$draft")"
if [ -n "$wrapped" ]; then
  echo "brand-check: prose wraps a sentence across physical lines (first at line $(printf '%s\n' "$wrapped" | head -1))" >&2
  fail=1
fi

# 4. Founder-name forms from brand-identity ## Founder. A `never:` form in the
#    draft fails brand review. The allowed (formal/byline) and banned (never)
#    forms are declared per-project in brand-identity, so this enforces the
#    name rule without hardcoding it into the pack voice.
if [[ -n "$identity" ]]; then
  never_names=()
  while IFS= read -r line; do [[ -n "$line" ]] && never_names+=("$line"); done < <(awk '
    /^##[[:space:]]+Founder([[:space:]]|$)/ { in_founder = 1; next }
    in_founder && /^##[[:space:]]/ { in_founder = 0 }
    in_founder && /^[[:space:]]*never:[[:space:]]*/ {
      line = $0
      sub(/^[[:space:]]*never:[[:space:]]*/, "", line)
      sub(/[[:space:]]*#.*$/, "", line)
      if (line != "") print line
    }
  ' "$identity")
  if [ "${#never_names[@]}" -gt 0 ]; then
    for name in "${never_names[@]}"; do
      if grep -qniF -- "$name" "$draft"; then
        echo "brand-check: banned founder-name form present: $name" >&2
        fail=1
      fi
    done
  fi
fi

exit $fail
