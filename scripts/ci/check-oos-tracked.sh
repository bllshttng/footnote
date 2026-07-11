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
#     (`<prefix>-<hex>`) or a carveout (`cv-<hex>`), or an inline `oos-ok:`. A
#     bullet/numbered line is one item; a section with no list is treated as a
#     single item (the whole prose block).
#
# Input (via env ONLY; a workflow must NEVER interpolate PR-controlled text
# inline into a `run:` block - that is a shell-injection vector):
#   PR_BODY   the PR body
#
# Exit 0 when clean (or no OOS section, or empty body); exit 1 on an untracked
# item. Absent/empty PR_BODY = nothing to gate = pass (mirrors loc-ratchet's
# "unset PR_BODY = no exception declared").
#
# Run locally (bash 3.2 safe - the maintainer's macOS /bin/bash):
#   PR_BODY="$(gh pr view <n> --json body -q .body)" bash scripts/ci/check-oos-tracked.sh

set -uo pipefail

BODY="${PR_BODY:-}"

# A tracked reference. A backlog node id is <prefix>-<hex>: the prefix grammar
# mirrors config.backlog.id_prefix (BacklogBlock.validate_id_prefix: a letter-led
# 1-7 char lowercase alnum token), so a configured prefix like `proj1-` is
# honored - NOT just the `ab-`/`x-` defaults. The hex tail is 4+ (x-ids are 4,
# ab-ids 8, id_hex_width configurable). Carveout ids (cv-<hex>) match the same
# shape. Deliberately does NOT count a bare `#123` GitHub ref as tracking - a
# PR/issue is not a backlog node; route those through `oos-ok: tracked in #123`.
REF='\b[a-z][a-z0-9]{0,6}-[0-9a-f]{4,}\b'
# An inline waiver on an item, or (standalone) on the whole section. Requires a
# non-empty rationale (a bare `oos-ok:` does not waive), like loc-exception:.
OOSOK='oos-ok:[[:space:]]*[^[:space:]]'

# match <ere> <string> [i] -> true if <string> matches. Uses grep WITHOUT -q so
# the whole input is consumed: under `set -o pipefail` a `grep -q` that exits on
# first match can SIGPIPE the upstream printf (exit 141) and fail the pipeline
# (gemini review). Small single-line inputs, so the full read is free.
match() {
  local flags='-E'
  [[ "${3:-}" == i ]] && flags='-iE'
  printf '%s' "$2" | grep $flags "$1" >/dev/null 2>&1
}

[[ -z "$BODY" ]] && { echo "check-oos-tracked: no PR body - nothing to gate"; exit 0; }

# --- 1. locate the "Out of scope" section ------------------------------------
# Heading forms (case-insensitive, markdown ATX heading only): "Out of scope",
# "Out-of-scope", "Not touched here". Section body runs to the next ATX heading
# or EOF. bash 3.2 safe: line-by-line, no mapfile.
in_section=0
section_lines=()
found_heading=0
while IFS= read -r line; do
  if match '^#{1,6}[[:space:]]' "$line"; then
    # a heading: does it open, or (if we were inside) close, the OOS section?
    if match '^#{1,6}[[:space:]]*(out.?of.?scope|not touched here)' "$line" i; then
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
# Guarded expansion (${a[@]+"${a[@]}"}): an empty array under `set -u` on bash
# 3.2 errors on a plain "${a[@]}" (gemini review) - an OOS heading at EOF.
for line in ${section_lines[@]+"${section_lines[@]}"}; do
  if match "$OOSOK" "$line" && ! match '^[[:space:]]*([-*+]|[0-9]+\.)[[:space:]]' "$line"; then
    echo "check-oos-tracked: section waived by 'oos-ok:' - ok"
    exit 0
  fi
done

# --- 3. collect items --------------------------------------------------------
# A list item (- / * / + / "1.") is one item. If the section has no list items,
# the whole non-blank prose block is a single item (the paragraph OOS case).
items=()
has_list=0
prose=""
for line in ${section_lines[@]+"${section_lines[@]}"}; do
  if match '^[[:space:]]*([-*+]|[0-9]+\.)[[:space:]]+[^[:space:]]' "$line"; then
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
for item in ${items[@]+"${items[@]}"}; do
  # tracked ref, or an inline oos-ok: with a non-empty rationale
  if match "$REF" "$item" i || match "$OOSOK" "$item" i; then
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
