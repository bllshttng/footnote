#!/usr/bin/env bash
# check-pitfalls.sh - CI gate over AGENTS.md's `## Pitfalls corpus (capped)`
# section. Keeps the corpus from becoming postmortems 2.0:
#   cap        more than 10 active `###` entries fails (context-cost budget)
#   fields     every entry needs a `graduates-to:` and an `added:` line
#   graduation a `graduates-to:` value must resolve to a filed backlog node:
#              id-shaped values by id, anything else by exact title match on
#              the value or any one of its sentences; no graph file skips
#   staleness  an `added:` date older than 60 days fails (graduate or evict);
#              when git history dates the entry's first appearance in the
#              corpus file, that date replaces the prose date and a mismatch
#              is reported
#   qualifier  a pinned claim-bearing phrase deleted from a live entry fails
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

# Each entry -> one newline-delimited TSV record:
# title<TAB>has_grad<TAB>has_added<TAB>date<TAB>grad_value.
# Newline-delimited reads are reliable across shells; the date regex avoids the
# {n} interval quantifier (absent on BSD awk). has_added is separate from date so
# "no added: line" and "added: line with no parseable date" report differently.
# grad_value carries the first `graduates-to:` value verbatim for the
# resolution check below.
ENTRY_COUNT=0
STALE_DATES=""
GRAD_LINES=""
TITLES=""
while IFS=$'\t' read -r title has_grad has_added date grad_val; do
  [[ -z "$title" ]] && continue
  ENTRY_COUNT=$((ENTRY_COUNT + 1))
  TITLES+="${title}"$'\n'
  GRAD_LINES+="${title}"$'\t'"${grad_val}"$'\n'
  [[ "$has_grad" != "1" ]] && add_violation "entry '${title}' is missing a 'graduates-to:' field"
  if [[ "$has_added" != "1" ]]; then
    add_violation "entry '${title}' is missing an 'added:' field"
  elif [[ -z "$date" ]]; then
    add_violation "entry '${title}' has an 'added:' line without a YYYY-MM-DD date"
  else
    STALE_DATES+="${title}"$'\t'"${date}"$'\n'
  fi
done < <(
  awk '
    /^### / {
      if (in_entry) { print title "\t" has_grad "\t" has_added "\t" date "\t" grad_val }
      title = $0; sub(/^### */, "", title); gsub(/\t/, " ", title)
      in_entry = 1; has_grad = "0"; has_added = "0"; date = ""; grad_val = ""
      next
    }
    in_entry && $0 ~ /^[ \t]*-?[ \t]*graduates-to:[ \t]*[^ \t]/ {
      has_grad = "1"
      if (grad_val == "") {
        grad_val = $0
        sub(/^[ \t]*-?[ \t]*graduates-to:[ \t]*/, "", grad_val)
        gsub(/\t/, " ", grad_val)
      }
    }
    in_entry && $0 ~ /^[ \t]*-?[ \t]*added:[ \t]*[^ \t]/ {
      has_added = "1"
      if (date == "" && match($0, /[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]/)) {
        date = substr($0, RSTART, RLENGTH)
      }
    }
    END { if (in_entry) { print title "\t" has_grad "\t" has_added "\t" date "\t" grad_val } }
  ' <<< "$SECTION"
)

if (( ENTRY_COUNT > MAX_ENTRIES )); then
  add_violation "${ENTRY_COUNT} entries exceed the ${MAX_ENTRIES}-entry cap; evict or graduate one in this PR"
fi

# Graduated-verb check: a title naming a shipped `fno` verb as the trap is by
# definition un-graduated, because the verb IS the carrier its `graduates-to:`
# is waiting for. This is the loop that produced the "`fno doctor test` can report a
# false green" entry - written the day after the verb that fixed it, then read
# at every SessionStart as a reason to distrust it. Titles only: a body may
# cite a verb as a specimen without claiming it is the trap.
#
# The verb list is read from the CLI's lazy registry rather than by running
# `fno`, so the gate stays hermetic and works on a checkout with nothing
# installed. A consumer repo shipping this gate without the CLI skips it.
REGISTRY="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/cli/src/fno/cli.py"
if [[ -n "$TITLES" && -f "$REGISTRY" ]]; then
  SHIPPED_VERBS=$(sed -n 's/^[[:space:]]*"\([a-z][a-z0-9-]*\)":[[:space:]]*(.*/\1/p' "$REGISTRY")
  # An empty list here is a BROKEN READ, not a CLI with no verbs. Every title
  # then matches nothing and the gate passes on anything, which is the shape
  # this gate exists to refuse. An ABSENT registry is the legitimate skip
  # (a consumer repo without the CLI) and is handled by the -f test above; a
  # PRESENT registry that yields nothing is an instrument failure. Fail loud.
  if [[ -z "$SHIPPED_VERBS" ]]; then
    echo "check-pitfalls: read 0 verbs from ${REGISTRY}." >&2
    echo "  The registry exists, so this is a broken read, not an empty CLI." >&2
    echo "  Refusing rather than passing every title against an empty list." >&2
    exit 1
  fi
  while IFS= read -r title; do
    [[ -z "$title" ]] && continue
    # Backticks are decoration around the verb, not part of it.
    probe="${title//\`/}"
    while IFS= read -r verb; do
      [[ -z "$verb" ]] && continue
      if printf '%s\n' "$SHIPPED_VERBS" | grep -qxF "$verb"; then
        add_violation "entry '${title}' names the shipped verb 'fno ${verb}'; a shipped verb is the carrier that graduates its entry, so evict the entry and let the verb (plus its --help) teach"
      fi
    done < <(printf '%s\n' "$probe" | grep -oE '(^|[^a-zA-Z])fno +[a-z][a-z0-9-]*' | sed -E 's/.*fno +//')
  done <<< "$TITLES"
fi

# Claim-bearing qualifiers: the words a structural check cannot see. Every
# check above counts something - headings, fields, dates, backticked tokens -
# and a qualifier is none of those. So a prose diet deletes the one word
# carrying an entry's claim and every count still passes. Measured on the
# corpus diet: eleven qualifiers went missing, and two of them REVERSED an
# entry into the defect it exists to teach against ("only the lockfile"
# says presence proves ownership; "evidenced only by mail" rejects a
# worker's own autonomous evidence).
#
# Each pin is entry-key<TAB>required-phrase, matched byte-exact. A pin binds
# only while its own entry is present, so evicting or graduating an entry
# releases its phrase instead of wedging this gate on prose the corpus is
# supposed to have dropped.
PINNED_PHRASES=$'capability probe\tmail probe'

while IFS=$'\t' read -r entry_key phrase; do
  [[ -z "$entry_key" || -z "$phrase" ]] && continue
  printf '%s' "$TITLES" | grep -qF -- "$entry_key" || continue
  printf '%s' "$SECTION" | grep -qF -- "$phrase" && continue
  add_violation "entry '${entry_key}' no longer carries the pinned qualifier '${phrase}'; that word carries the claim, and every structural check above passes without it. Restore it, or drop its pin in this PR when the entry deliberately stops making that claim"
done <<< "$PINNED_PHRASES"

# Resolution: a `graduates-to:` value is written by the same hand that owns
# the corpus, and a gate whose input is a field the guarded party writes does
# not measure the guarded party. So the value must resolve against the backlog
# graph at run time. An id-shaped value resolves by node id; anything else
# resolves when the value, or any one sentence of it, equals a filed node's
# title after whitespace-collapse, trailing-period strip, and case-folding.
# No graph file is the legitimate skip (a consumer repo without the backlog);
# a graph that exists but cannot be read fails loud, on the shipped-verb
# registry precedent above.
GRAPH_JSON="${FNO_GRAPH_JSON:-${HOME}/.fno/graph.json}"
if [[ -n "$GRAD_LINES" && -f "$GRAPH_JSON" ]]; then
  if ! GRAD_REPORT="$(GRAD_LINES="$GRAD_LINES" GRAPH_JSON="$GRAPH_JSON" python3 - <<'PY'
import json, os, re, sys
node_id = re.compile(r"\b(?:x-[0-9a-f]{4}|ab-[0-9a-f]{8})\b")

def norm(s):
    return " ".join(s.split()).rstrip(".").casefold()

try:
    with open(os.environ["GRAPH_JSON"], encoding="utf-8") as fh:
        graph = json.load(fh)
except (OSError, ValueError) as exc:
    sys.stderr.write(
        "check-pitfalls: the backlog graph at %s cannot be read (%s).\n"
        "  graduates-to: values must resolve against it; refusing to pass\n"
        "  every entry against an unreadable graph.\n" % (os.environ["GRAPH_JSON"], exc)
    )
    sys.exit(1)

ids, titles = set(), set()
for n in graph.get("entries", []):
    if isinstance(n, dict):
        if isinstance(n.get("id"), str):
            ids.add(n["id"])
        if isinstance(n.get("title"), str):
            titles.add(norm(n["title"]))

for rec in os.environ["GRAD_LINES"].splitlines():
    if not rec or "\t" not in rec:
        continue
    title, value = rec.split("\t", 1)
    sentences = [s for s in re.split(r"(?<=\.)\s+", value) if s] or [value]
    if any(set(node_id.findall(s)) & ids for s in sentences):
        continue
    if any(norm(s) in titles for s in sentences):
        continue
    print(
        f"entry '{title}' has a 'graduates-to:' value that resolves to no filed "
        f"node: '{value}'. File the guard (its title is this value), then the "
        f"entry resolves."
    )
PY
  )"; then
    exit 1  # the python pass already explained the unreadable graph on stderr
  fi
  while IFS= read -r line; do
    [[ -n "$line" ]] && add_violation "$line"
  done <<< "$GRAD_REPORT"
fi

# Git cross-check of the prose `added:` date. The date is chosen by the same
# hand that owns the corpus; where git history can answer when the entry first
# appeared in the corpus file, git wins: that date replaces the prose date for
# the staleness verdict, and a mismatch is reported. The search is scoped to
# the corpus file so a title string echoing elsewhere in history cannot forge
# an older date, and a target with no git history (a fixture, or a repo where
# the file is newer than the clone) simply keeps its prose date.
GIT_DATES=""
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GIT_TARGET="$TARGET"
case "$GIT_TARGET" in
  "$REPO_ROOT"/*) GIT_TARGET="${GIT_TARGET#"$REPO_ROOT"/}" ;;
  /*) GIT_TARGET="" ;;
esac
if [[ -n "$GIT_TARGET" ]] && git -C "$REPO_ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  while IFS= read -r title; do
    [[ -z "$title" ]] && continue
    gdate="$(git -C "$REPO_ROOT" log --format=%cs -S"$title" -- "$GIT_TARGET" 2>/dev/null | tail -1)"
    [[ -z "$gdate" ]] && continue
    GIT_DATES+="${title}"$'\t'"${gdate}"$'\n'
  done <<< "$TITLES"
fi

# Staleness: one python3 pass over the collected dates (portable date math),
# with the git first-appearance date overriding the prose date where history
# has one.
if [[ -n "$STALE_DATES" ]]; then
  STALE_REPORT="$(STALE_DATES="$STALE_DATES" GIT_DATES="$GIT_DATES" GIT_TARGET="$GIT_TARGET" MAX_AGE_DAYS="$MAX_AGE_DAYS" python3 - <<'PY'
import os, sys, datetime
today = datetime.datetime.now(datetime.timezone.utc).date()
max_age = int(os.environ["MAX_AGE_DAYS"])
git_dates = {}
for rec in os.environ.get("GIT_DATES", "").splitlines():
    if rec and "\t" in rec:
        t, g = rec.split("\t", 1)
        git_dates[t] = g
out = []
for rec in os.environ["STALE_DATES"].splitlines():
    rec = rec.strip()
    if not rec or "\t" not in rec:
        continue
    title, date = rec.split("\t", 1)
    eff = date
    if title in git_dates:
        eff = git_dates[title]
        if eff != date:
            print(
                f"note: entry '{title}' added: says {date} but first appeared "
                f"in {os.environ.get('GIT_TARGET') or 'the corpus file'} on "
                f"{eff}; the git date decides staleness",
                file=sys.stderr,
            )
    try:
        d = datetime.date.fromisoformat(eff)
    except ValueError:
        out.append(f"entry '{title}' has an unparseable added date '{eff}'")
        continue
    age = (today - d).days
    if age > max_age:
        src = "git first appearance" if title in git_dates else "added:"
        out.append(
            f"entry '{title}' is {age} days old ({eff}, {src}), over the "
            f"{max_age}-day limit; graduate or evict"
        )
print("\n".join(out))
PY
)"
  while IFS= read -r line; do
    [[ -n "$line" ]] && add_violation "$line"
  done <<< "$STALE_REPORT"
fi

# Byte-budget awareness (x-62e1): the entry-count cap and the preamble byte
# ceiling measure the same SessionStart context cost in different units, and
# only the byte ceiling binds against the real budget. An entry that passes the
# count cap can still blow the byte ceiling, so the count alone advertised
# capacity that did not exist. Reuse the canonical measurement
# (check-preamble-budget) so the two gates share one file set and ceiling, then
# report the byte-bound remaining capacity and refuse when the preamble is over
# the ceiling - the failure then lands in this gate, the one a pitfalls edit
# works in, not only in check-preamble-budget.
PRE_SPARE=""
PRE_FIT=""
PREAMBLE_BUDGET_SH="$(dirname "${BASH_SOURCE[0]}")/check-preamble-budget.sh"
PRE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# The byte half applies to the REPO's own preamble, so it runs only when TARGET
# is that file. A fixture path parses a corpus that is not the preamble, and
# reporting this repo's bytes beside it made one message describe two different
# trees - and, at zero spare, failed every fixture test on an unrelated ceiling.
CHECK_BYTES=0
if [[ "$(cd "$(dirname "$TARGET")" && pwd)/$(basename "$TARGET")" == "$PRE_ROOT/AGENTS.md" ]]; then
  CHECK_BYTES=1
fi

if (( CHECK_BYTES )); then
  # Fail CLOSED on a reading we could not take. Passing the root fixed the $PWD
  # trigger, but the SHAPE was the defect: any unmatched read left PRE_SPARE
  # empty, the refusal below no-opped, and the run printed "all valid" with the
  # byte gate silently gone. This gate declares that it consults the byte
  # budget, so a run that could not consult it is not a pass.
  if [[ ! -f "$PREAMBLE_BUDGET_SH" ]]; then
    echo "check-pitfalls: cannot consult the byte budget: $PREAMBLE_BUDGET_SH is missing." >&2
    echo "  This gate reports preamble headroom, so a run that cannot read it is not a pass." >&2
    exit 1
  fi
  PRE_QUIET="$(bash "$PREAMBLE_BUDGET_SH" --quiet "$PRE_ROOT" 2>&1 || true)"
  if [[ "$PRE_QUIET" =~ preamble:\ ([0-9]+)\ /\ ([0-9]+)\ B ]]; then
    PRE_TOTAL="${BASH_REMATCH[1]}"
    PRE_CEIL="${BASH_REMATCH[2]}"
    PRE_SPARE=$((PRE_CEIL - PRE_TOTAL))
    # Floor for one formatted entry (heading + 1-3 sentence trap + specimens +
    # graduates-to + added). Real entries run 600-900 B; the floor is the
    # smallest that still satisfies the format, so "N fit" never overstates -
    # the message names the 400-B floor so the assumption is explicit.
    if (( PRE_SPARE >= 0 )); then
      PRE_FIT=$(( PRE_SPARE / 400 ))
    else
      PRE_FIT=0
    fi
  else
    echo "check-pitfalls: could not read a byte verdict from $PREAMBLE_BUDGET_SH." >&2
    echo "  Its --quiet output did not carry the expected 'preamble: N / M B' line:" >&2
    echo "    ${PRE_QUIET:-(no output)}" >&2
    echo "  Refusing rather than reporting 'all valid' with the byte gate absent." >&2
    exit 1
  fi
fi

if [[ -n "$PRE_SPARE" && $PRE_SPARE -lt 0 ]]; then
  {
    echo "check-pitfalls: ${ENTRY_COUNT}/${MAX_ENTRIES} entries, but the preamble is $((-PRE_SPARE)) B over the byte ceiling."
    echo "  The count cap is not the binding constraint here: the SessionStart preamble"
    echo "  byte budget (check-preamble-budget.sh) is. Corpus growth or unrelated"
    echo "  AGENTS.md growth pushed it over."
    echo "  Fix: cut bytes from AGENTS.md or another preamble file, or raise"
    echo "  CEILING_BYTES in scripts/ci/check-preamble-budget.sh in this PR with"
    echo "  the reason in the PR body."
  } >&2
  exit 1
fi

CAP_SUFFIX=""
[[ -n "$PRE_FIT" ]] && CAP_SUFFIX="; ${PRE_SPARE} B preamble headroom (~${PRE_FIT} more fit at the 400-B floor)"

if [[ $VIOLATIONS -eq 0 ]]; then
  echo "check-pitfalls: ${ENTRY_COUNT}/${MAX_ENTRIES} entries, all valid${CAP_SUFFIX}"
  exit 0
fi

{
  echo "check-pitfalls: ${VIOLATIONS} violation(s) in '${SECTION_HEADER}'${CAP_SUFFIX}"
  echo
  printf '%s' "$REPORT"
  echo
  echo "Fix: a landed graduates-to guard removes its entry in the same PR;"
  echo "  over ${MAX_ENTRIES} entries -> evict or graduate one; older than"
  echo "  ${MAX_AGE_DAYS} days -> graduate to a lint or evict."
} >&2
exit 1
