#!/usr/bin/env bash
# brand-check.sh <draft> [brand-identity]
#
# A deterministic STRUCTURAL voice check. Fails when the draft contains a
# banned term (read from the 'Don't' block of assets/brand-voice.md, so the
# asset is the rule source and this script is only the enforcement), an em dash
# (U+2014), or a prose paragraph that wraps one sentence across physical lines.
# When a brand-identity path is given, also fails on a founder-name form
# declared `never:` under its `## Founder` block: the per-project name rule,
# enforced rather than left to prose. Omitting brand-identity skips the
# founder-name check and warns, so the skip is visible rather than silent.
#
# POSIX-portable: no mapfile (bash 3.2), no grep -P (BSD grep). The em dash is
# matched by its UTF-8 byte sequence so no literal em dash appears in source.
# Empty arrays are guarded so expansion under `set -u` does not crash bash 3.2.
set -euo pipefail

draft="${1:?usage: brand-check.sh <draft> [brand-identity]}"
[[ -f "$draft" ]] || { echo "brand-check: draft not found: $draft" >&2; exit 2; }

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
voice="$script_dir/../assets/brand-voice.md"
[[ -f "$voice" ]] || { echo "brand-check: brand-voice asset not found: $voice" >&2; exit 2; }

identity="${2:-}"
never_names=()
allowed_names=()
if [[ -z "$identity" ]]; then
  echo "brand-check: no brand-identity given; founder-name forms not enforced" >&2
elif [[ ! -f "$identity" ]]; then
  echo "brand-check: brand-identity not found: $identity" >&2; exit 2
else
  while IFS=$'\t' read -r kind val; do
    [[ -z "$val" ]] && continue
    case "$kind" in
      never) never_names+=("$val") ;;
      formal|byline) allowed_names+=("$val") ;;
    esac
  done < <(awk '
    /^##[[:space:]]+Founder([[:space:]]|$)/ { in_founder = 1; next }
    in_founder && /^##[[:space:]]/ { in_founder = 0 }
    in_founder && /^[[:space:]]*(formal|byline|never):[[:space:]]*/ {
      line = $0
      sub(/^[[:space:]]*/, "", line)
      kind = line
      sub(/:.*$/, "", kind)
      val = line
      sub(/^[^:]*:[[:space:]]*/, "", val)
      sub(/[[:space:]]*#.*$/, "", val)
      if (val != "") print kind "\t" val
    }
  ' "$identity")
  # A never-form that is a substring of a declared allowed-form would
  # false-positive on valid drafts; warn and drop it rather than arm the trap.
  if [ "${#never_names[@]}" -gt 0 ] && [ "${#allowed_names[@]}" -gt 0 ]; then
    cleaned=()
    for nv in "${never_names[@]}"; do
      clash=0
      nvl="$(printf '%s' "$nv" | tr '[:upper:]' '[:lower:]')"
      for av in "${allowed_names[@]}"; do
        avl="$(printf '%s' "$av" | tr '[:upper:]' '[:lower:]')"
        case "$avl" in *"$nvl"*) clash=1; break ;; esac
      done
      if [ "$clash" -eq 1 ]; then
        echo "brand-check: never form '$nv' is a substring of an allowed form; not enforced" >&2
      else
        cleaned+=("$nv")
      fi
    done
    never_names=("${cleaned[@]+"${cleaned[@]}"}")
  fi
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

# 3. No sentence split across physical lines. A prose line that does not end in
#    terminal punctuation (. ! ? : ;) is a sentence split across lines, because a
#    complete sentence on one line always terminates.
#
#    This catches a hard wrap and nothing else. The house rule is stricter: a
#    paragraph is ONE physical line, so a newline after every period breaks it
#    too, and this check passes such a draft. Rule 6 of `fno doctor lint style` is what
#    refuses that shape. Both agree on the case here, so the checks compose.
#
#    Skip headings, list items, tables, code fences, image lines, blank lines,
#    and a standalone founder-name line (a byline is a signature, not a wrapped
#    sentence). The comparison is case-insensitive, so a byline emitted in a
#    different case from the declared form is still skipped. Allowed names are
#    newline-delimited, because a comma in a form would corrupt a CSV join.
allowed_nl=""
if [ "${#allowed_names[@]}" -gt 0 ]; then
  allowed_nl="$(printf '%s\n' "${allowed_names[@]}")"
fi
export allowed_nl
wrapped="$(awk '
  BEGIN { infence = 0; n = split(ENVIRON["allowed_nl"], a, "\n"); for (i=1; i<=n; i++) if (a[i] != "") allowed_set[tolower(a[i])] = 1 }
  /^[[:space:]]*$/ { next }
  /^```/ { infence = !infence; next }
  infence { next }
  /^[#]/ { next }
  /^[[:space:]]*[-*+][[:space:]]/ { next }
  /^[[:space:]]*\|/ { next }
  /^[[:space:]]*!\[/ { next }
  {
    trimmed = $0
    gsub(/^[[:space:]]+|[[:space:]]+$/, "", trimmed)
    if (tolower(trimmed) in allowed_set) next
    last = substr($0, length($0), 1)
    if (last !~ /[.!?:;]/) { print NR }
  }
' "$draft")"
if [ -n "$wrapped" ]; then
  echo "brand-check: prose wraps a sentence across physical lines (first at line $(printf '%s\n' "$wrapped" | head -1))" >&2
  fail=1
fi

# 4. Founder-name forms from brand-identity ## Founder. A `never:` form in the
#    draft fails brand review; `formal`/`byline` pass (and are skipped as
#    signature lines in the wrapped-prose check). Declared per-project, so this
#    enforces the name rule without hardcoding it into the pack voice.
if [ "${#never_names[@]}" -gt 0 ]; then
  for name in "${never_names[@]}"; do
    if grep -qniF -- "$name" "$draft"; then
      echo "brand-check: banned founder-name form present: $name" >&2
      fail=1
    fi
  done
fi

exit $fail
