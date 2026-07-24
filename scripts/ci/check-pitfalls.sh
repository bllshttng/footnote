#!/usr/bin/env bash
# check-pitfalls.sh - CI gate over AGENTS.md's `## Pitfalls corpus (capped)`
# section. Keeps the corpus from becoming postmortems 2.0:
#   cap        more than 10 active `###` entries fails (context-cost budget)
#   fields     every entry needs a `graduates-to:` and an `added:` line
#   staleness  an `added:` date older than 60 days fails (graduate or evict)
#
# Why a cap at all: AGENTS.md is injected at every SessionStart on every
# harness, so each entry is paid on every session on every lane. An entry too
# large to fit the format graduates to a lint instead of joining the corpus.
# The cap-race window (two same-day PRs each adding a 10th) is caught at the
# next choke-point pass; this gate runs on every PR that touches AGENTS.md.
#
# Run: bash scripts/ci/check-pitfalls.sh [markdown-path]
# Default target AGENTS.md. Exits 0 clean; exits 1 with a report otherwise.

set -euo pipefail

TARGET="${1:-AGENTS.md}"
MAX_ENTRIES=10
MAX_AGE_DAYS=60
SECTION_HEADER='## Pitfalls corpus (capped)'

[[ -f "$TARGET" ]] || { echo "check-pitfalls: target not found: $TARGET" >&2; exit 1; }

# Section body: from the header line up to (not including) the next ^## heading.
# Exact-line match, not regex: the header's parens are regex-special.
SECTION=$(awk -v hdr="$SECTION_HEADER" '
  $0 == hdr { in_sec = 1; next }
  in_sec && /^## / { in_sec = 0 }
  in_sec { print }
' "$TARGET")

if [[ -z "$SECTION" ]]; then
  {
    echo "check-pitfalls: no '${SECTION_HEADER}' section in ${TARGET}."
    echo "  Any repo shipping this gate must also ship the section."
  } >&2
  exit 1
fi

VIOLATIONS=0
REPORT=""
add_violation() {
  REPORT+="[pitfalls] $1"$'\n'
  VIOLATIONS=$((VIOLATIONS + 1))
}

# Each entry -> one newline-delimited TSV record: title<TAB>has_grad<TAB>has_added<TAB>date.
# Newline-delimited reads are reliable across shells; the date regex avoids the
# {n} interval quantifier (absent on BSD awk). has_added is separate from date so
# "no added: line" and "added: line with no parseable date" report differently.
ENTRY_COUNT=0
STALE_DATES=""
while IFS=$'\t' read -r title has_grad has_added date; do
  [[ -z "$title" ]] && continue
  ENTRY_COUNT=$((ENTRY_COUNT + 1))
  [[ "$has_grad" != "1" ]] && add_violation "entry '${title}' is missing a 'graduates-to:' field"
  if [[ "$has_added" != "1" ]]; then
    add_violation "entry '${title}' is missing an 'added:' field"
  elif [[ -z "$date" ]]; then
    add_violation "entry '${title}' has an 'added:' line without a YYYY-MM-DD date"
  else
    STALE_DATES+="${title}|${date}"$'\n'
  fi
done < <(
  awk '
    /^### / {
      if (in_entry) { print title "\t" has_grad "\t" has_added "\t" date }
      title = $0; sub(/^### */, "", title); gsub(/\t/, " ", title)
      in_entry = 1; has_grad = "0"; has_added = "0"; date = ""
      next
    }
    in_entry && $0 ~ /^[ \t]*-?[ \t]*graduates-to:/ { has_grad = "1" }
    in_entry && $0 ~ /^[ \t]*-?[ \t]*added:/ {
      has_added = "1"
      if (date == "" && match($0, /[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]/)) {
        date = substr($0, RSTART, RLENGTH)
      }
    }
    END { if (in_entry) { print title "\t" has_grad "\t" has_added "\t" date } }
  ' <<< "$SECTION"
)

if (( ENTRY_COUNT > MAX_ENTRIES )); then
  add_violation "${ENTRY_COUNT} entries exceed the ${MAX_ENTRIES}-entry cap; evict or graduate one in this PR"
fi

# Staleness: one python3 pass over the collected dates (portable date math).
if [[ -n "$STALE_DATES" ]]; then
  STALE_REPORT="$(STALE_DATES="$STALE_DATES" MAX_AGE_DAYS="$MAX_AGE_DAYS" python3 - <<'PY'
import os, datetime
today = datetime.date.today()
max_age = int(os.environ["MAX_AGE_DAYS"])
out = []
for rec in os.environ["STALE_DATES"].splitlines():
    rec = rec.strip()
    if not rec or "|" not in rec:
        continue
    title, date = rec.split("|", 1)
    try:
        d = datetime.date.fromisoformat(date)
    except ValueError:
        out.append(f"entry '{title}' has an unparseable added date '{date}'")
        continue
    age = (today - d).days
    if age > max_age:
        out.append(f"entry '{title}' is {age} days old (added {date}), over the {max_age}-day limit; graduate or evict")
print("\n".join(out))
PY
)"
  while IFS= read -r line; do
    [[ -n "$line" ]] && add_violation "$line"
  done <<< "$STALE_REPORT"
fi

if [[ $VIOLATIONS -eq 0 ]]; then
  echo "check-pitfalls: ${ENTRY_COUNT}/${MAX_ENTRIES} entries, all valid"
  exit 0
fi

{
  echo "check-pitfalls: ${VIOLATIONS} violation(s) in '${SECTION_HEADER}'"
  echo
  printf '%s' "$REPORT"
  echo
  echo "Fix: a landed graduates-to guard removes its entry in the same PR;"
  echo "  over ${MAX_ENTRIES} entries -> evict or graduate one; older than"
  echo "  ${MAX_AGE_DAYS} days -> graduate to a lint or evict."
} >&2
exit 1
