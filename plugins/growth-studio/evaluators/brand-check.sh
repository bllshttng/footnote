#!/usr/bin/env bash
# brand-check.sh <draft>
#
# A deterministic STRUCTURAL voice check. Fails when the draft contains a
# banned term (read from the 'Don't' block of assets/brand-voice.md, so the
# asset is the rule source and this script is only the enforcement), an em dash
# (U+2014), or a prose paragraph that wraps one sentence across physical lines.
#
# POSIX-portable: no mapfile (bash 3.2), no grep -P (BSD grep). The em dash is
# matched by its UTF-8 byte sequence so no literal em dash appears in source.
set -euo pipefail

draft="${1:?usage: brand-check.sh <draft>}"
[[ -f "$draft" ]] || { echo "brand-check: draft not found: $draft" >&2; exit 2; }

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
voice="$script_dir/../assets/brand-voice.md"
[[ -f "$voice" ]] || { echo "brand-check: brand-voice asset not found: $voice" >&2; exit 2; }

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

exit $fail
