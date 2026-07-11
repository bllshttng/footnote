#!/usr/bin/env bash
# check-oos-tracked.sh - CI gate: every item in a PR body's "Out of scope"
# section must reference a tracked backlog node / carveout (or be explicitly
# waived), so deferred work is never lost as write-only PR prose.
#
# The failure this prevents: a PR body says "out of scope: X" and X exists
# nowhere else - no backlog node, no carveout. Once the PR merges the body is
# archival prose that no `fno backlog` query, roadmap, or nag ever surfaces
# again, so the deferred work silently evaporates.
#
# NARROW by design: it fires ONLY when the body has an explicit "Out of scope"
# (or "Not touched here") heading. Incidental prose elsewhere is never scanned -
# the section heading is the smell; free-text deferral phrasing is not gated.
#
# Rule inside that section:
#   - A standalone `oos-ok: <rationale>` line waives the WHOLE section (mirrors
#     loc-ratchet's `loc-exception:`) - for genuinely-nothing-to-track cases.
#   - Otherwise every ITEM must carry a tracked ref on its own text: a node id
#     (`x-abcd` / `ab-1234abcd` / any `<prefix>-<hex>`) or a carveout (`cv-...`),
#     or an inline `oos-ok:`. A bullet/numbered line is one item; a section with
#     no list is treated as a single item (the whole prose block).
#
# Input (via env ONLY; a workflow must NEVER interpolate PR-controlled text
# inline into a `run:` block - that is a shell-injection vector):
#   PR_BODY   the PR body
#
# Exit 0 when clean (or no OOS section, or empty body); exit 1 on an untracked
# item. Absent/empty PR_BODY = nothing to gate = pass (mirrors loc-ratchet's
# "unset PR_BODY = no exception declared").
#
# Run locally:
#   PR_BODY="$(gh pr view <n> --json body -q .body)" bash scripts/ci/check-oos-tracked.sh

set -uo pipefail

BODY="${PR_BODY:-}"

# A tracked reference: a backlog node id (<prefix>-<hex>, prefix config-driven so
# match generically: 1-4 lowercase letters, a dash, 4-8 hex) or a carveout id.
# Deliberately does NOT count a bare `#123` GitHub ref as tracking - a PR/issue is
# not a backlog node; route those through `oos-ok: tracked in #123` instead.
REF='(\b[a-z]{1,4}-[0-9a-f]{4,8}\b|\bcv-[0-9a-f]{4,}\b)'
# An inline waiver on an item, or (standalone) on the whole section.
OOSOK='^[[:space:]]*oos-ok:[[:space:]]*[^[:space:]]'

[[ -z "$BODY" ]] && { echo "check-oos-tracked: no PR body - nothing to gate"; exit 0; }

# --- 1. locate the "Out of scope" section ------------------------------------
# Heading forms (case-insensitive, markdown ATX heading only): "Out of scope",
# "Out-of-scope", "Not touched here". Section body runs to the next ATX heading
# or EOF. bash 3.2 safe: line-by-line, no mapfile.
in_section=0
section_lines=()
found_heading=0
while IFS= read -r line; do
  low="$(printf '%s' "$line" | tr '[:upper:]' '[:lower:]')"
  if printf '%s' "$low" | grep -qE '^#{1,6}[[:space:]]'; then
    # a heading: does it open, or (if we were inside) close, the OOS section?
    if printf '%s' "$low" | grep -qE '^#{1,6}[[:space:]]*(out.?of.?scope|not touched here)'; then
      in_section=1; found_heading=1; continue
    elif [[ "$in_section" -eq 1 ]]; then
      in_section=0   # next heading ends the section
    fi
  fi
  [[ "$in_section" -eq 1 ]] && section_lines+=("$line")
done <<< "$BODY"

if [[ "$found_heading" -eq 0 ]]; then
  echo "check-oos-tracked: no 'Out of scope' section - nothing to gate"
  exit 0
fi

# --- 2. section-level waiver -------------------------------------------------
# A standalone `oos-ok:` line (not inside a list item) waives the whole section.
for line in "${section_lines[@]}"; do
  if printf '%s' "$line" | grep -qE "$OOSOK" \
     && ! printf '%s' "$line" | grep -qE '^[[:space:]]*([-*+]|[0-9]+\.)[[:space:]]'; then
    echo "check-oos-tracked: section waived by 'oos-ok:' - ok"
    exit 0
  fi
done

# --- 3. collect items --------------------------------------------------------
# A list item (- / * / + / "1.") is one item. If the section has no list items,
# the whole non-blank prose block is a single item (my paragraph OOS case).
items=()
has_list=0
prose=""
for line in "${section_lines[@]}"; do
  if printf '%s' "$line" | grep -qE '^[[:space:]]*([-*+]|[0-9]+\.)[[:space:]]+[^[:space:]]'; then
    has_list=1
    items+=("$line")
  elif [[ -n "${line//[[:space:]]/}" ]]; then
    prose+="$line "
  fi
done
if [[ "$has_list" -eq 0 ]]; then
  # no bullets: the whole prose block is one item (empty -> nothing to gate)
  [[ -z "${prose//[[:space:]]/}" ]] && { echo "check-oos-tracked: empty 'Out of scope' section - ok"; exit 0; }
  items+=("$prose")
fi

# --- 4. every item needs a tracked ref or inline oos-ok ----------------------
violations=0
report=""
for item in "${items[@]}"; do
  # tracked ref, or an inline oos-ok: with a non-empty rationale (a bare
  # `oos-ok:` with nothing after does NOT waive - same rule as the section level)
  if printf '%s' "$item" | grep -qiE "$REF" \
     || printf '%s' "$item" | grep -qiE 'oos-ok:[[:space:]]*[^[:space:]]'; then
    continue
  fi
  trimmed="$(printf '%s' "$item" | sed -E 's/^[[:space:]]*//; s/[[:space:]]*$//' | cut -c1-100)"
  report+="  > ${trimmed}"$'\n'
  violations=$((violations + 1))
done

if [[ "$violations" -eq 0 ]]; then
  echo "check-oos-tracked: all 'Out of scope' items are tracked - ok"
  exit 0
fi

{
  echo "check-oos-tracked: $violations 'Out of scope' item(s) with no tracked reference:"
  echo
  printf '%s' "$report"
  echo
  echo "A deferred-work item stated only in a PR body is lost on merge - no backlog"
  echo "query, roadmap, or nag surfaces it. Each 'Out of scope' item must point to"
  echo "tracked work. To fix, per item:"
  echo "  - file a node:   fno backlog idea \"<the deferred work>\"   then add its id"
  echo "                   to the item, e.g. '... - tracked as x-1a2b'"
  echo "  - or a carveout: fno carveout add --kind deferred \"<...>\"  (harvested at merge)"
  echo "  - or, if there is genuinely nothing to track (already covered elsewhere),"
  echo "    waive it: add 'oos-ok: <why nothing to track>' on the item, or one"
  echo "    standalone 'oos-ok: <rationale>' line to waive the whole section."
} >&2
exit 1
