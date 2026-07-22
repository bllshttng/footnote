#!/usr/bin/env bash
# backfill-plan.sh - deterministic scaffolding for the Plan Mode backfill adapter.
#
# Turns a native Plan Mode plan into a gate-passing fno design doc. The one
# genuinely new piece of reasoning - synthesizing ## Failure Modes and the 5 BDD
# Acceptance Criteria from the native plan's intent - is LLM-powered and lives in
# the /target skill body (see skills/target/SKILL.md and references/plan-mode-backfill.md).
# This script owns only the deterministic parts so they can be unit-tested:
#
#   skeleton <native-plan-file> <out-doc> [--slug S] [--title T]
#       Wrap the native plan in a design-doc skeleton + inline frontmatter
#       (status: design, so /blueprint accepts it). The native body is preserved
#       VERBATIM. Prints `has_failure_modes=yes|no` and
#       `has_acceptance_criteria=yes|no` so the caller knows what to synthesize
#       (a section already present is reused, never duplicated).
#
#   check-sections <doc>
#       Validate the gate-required structure: a ## Failure Modes heading with the
#       four sub-labels (Boundaries/Errors/Invariants/Concurrency) and a
#       ## Acceptance Criteria section carrying all 5 BDD AC types
#       (HP/ERR/UI/EDGE/FR). Prints each MISSING item (one per line, prefixed
#       `missing: `) so a retry can re-synthesize ONLY the rejected piece.
#       Exit 0 = complete; exit 1 = something missing.
#
#   render-diff <native-plan-file> <enriched-doc>
#       Print, for the confirm step, the sections the backfill ADDED, visually
#       distinct from the original native plan body.
#
# Exit codes: 0 ok; 1 validation failed (check-sections) / section missing;
#             2 usage / unreadable input.

set -uo pipefail

die() { echo "backfill-plan: $*" >&2; exit 2; }

# Derive a kebab slug from a markdown file's first heading or first non-empty line.
_derive_slug() {
  local f="$1"
  grep -m1 -E '[^[:space:]]' "$f" 2>/dev/null \
    | sed -E 's/^#{1,6}[[:space:]]+//; s/[^A-Za-z0-9]+/-/g; s/^-+//; s/-+$//' \
    | tr '[:upper:]' '[:lower:]' | cut -c1-60
}

# Derive a human title from the first heading, else the first non-empty line.
_derive_title() {
  local f="$1"
  grep -m1 -E '[^[:space:]]' "$f" 2>/dev/null | sed -E 's/^#{1,6}[[:space:]]+//'
}

cmd_skeleton() {
  local native="" out="" slug="" title=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --slug)  slug="$2"; shift 2 ;;
      --title) title="$2"; shift 2 ;;
      *) if [[ -z "$native" ]]; then native="$1"; elif [[ -z "$out" ]]; then out="$1"; fi; shift ;;
    esac
  done
  [[ -n "$native" && -n "$out" ]] || die "usage: skeleton <native-plan-file> <out-doc> [--slug S] [--title T]"
  [[ -r "$native" ]] || die "native plan not readable: $native"

  [[ -n "$slug" ]]  || slug="$(_derive_slug "$native")"
  [[ -n "$slug" ]]  || slug="plan-mode"
  [[ -n "$title" ]] || title="$(_derive_title "$native")"
  [[ -n "$title" ]] || title="Plan Mode plan"

  # Detect pre-existing required sections so the caller reuses, not duplicates.
  local has_fm="no" has_ac="no"
  grep -qE '^## Failure Modes[[:space:]]*$'      "$native" && has_fm="yes"
  grep -qE '^## Acceptance Criteria[[:space:]]*$' "$native" && has_ac="yes"

  local created; created="$(date -u +%Y-%m-%d)"
  local tmp="$out.tmp.$$"
  # Inline-list frontmatter only (stdlib reader cannot parse block lists).
  # status: design so /blueprint will accept it and transition design -> ready.
  # Quote the title: a derived heading like "Fix auth: redirect flow" contains
  # ': ' which is NOT a valid unquoted YAML scalar and would make /blueprint's
  # YAML frontmatter parser reject the backfilled doc. Escape \ then " for a
  # double-quoted YAML scalar.
  local title_esc="${title//\\/\\\\}"; title_esc="${title_esc//\"/\\\"}"
  {
    printf -- '---\n'
    printf 'title: "%s"\n' "$title_esc"
    printf 'status: design\n'
    printf 'source: claude-plan-mode\n'
    printf 'created_at: %s\n' "$created"
    printf 'slug: %s\n' "$slug"
    printf 'messaged_peers: []\n'
    printf 'executor: do\n'
    printf 'scope: single-project\n'
    printf -- '---\n\n'
    # Native plan body, VERBATIM. cat preserves it byte-for-byte.
    cat "$native"
    # Guarantee a trailing newline boundary before any appended sections.
    printf '\n'
  } > "$tmp" || die "failed writing skeleton to $tmp"
  mv -f "$tmp" "$out" || { rm -f "$tmp"; die "failed moving skeleton into $out"; }

  echo "has_failure_modes=$has_fm"
  echo "has_acceptance_criteria=$has_ac"
}

cmd_check_sections() {
  local doc="${1:-}"
  [[ -n "$doc" ]] || die "usage: check-sections <doc>"
  [[ -r "$doc" ]] || die "doc not readable: $doc"

  local missing=()

  # ## Failure Modes heading (the literal /blueprint hard-refuses without).
  if ! grep -qE '^## Failure Modes[[:space:]]*$' "$doc"; then
    missing+=("failure-modes-heading")
  fi
  # The four Failure Modes sub-labels (bold labels, the /think format).
  local label
  for label in Boundaries Errors Invariants Concurrency; do
    grep -qE "\\*\\*${label}\\*\\*" "$doc" || missing+=("failure-modes-sublabel:${label}")
  done

  # ## Acceptance Criteria heading.
  if ! grep -qE '^## Acceptance Criteria[[:space:]]*$' "$doc"; then
    missing+=("acceptance-criteria-heading")
  fi
  # The five BDD AC type suffixes (e.g. AC1-HP, AC2-ERR, ...). Use an explicit
  # boundary class instead of \b: \b is a GNU-grep extension and behaves
  # differently under BSD grep, which would make this gate platform-divergent.
  # (...|$) is identical on GNU and BSD ERE.
  local t
  for t in HP ERR UI EDGE FR; do
    grep -qE "AC[0-9]+-${t}([^A-Za-z0-9]|$)" "$doc" || missing+=("ac-type:${t}")
  done

  if [[ ${#missing[@]} -eq 0 ]]; then
    echo "ok: all required sections present"
    return 0
  fi
  local m
  for m in "${missing[@]}"; do echo "missing: $m"; done
  return 1
}

cmd_render_diff() {
  local native="${1:-}" enriched="${2:-}"
  [[ -n "$native" && -n "$enriched" ]] || die "usage: render-diff <native-plan-file> <enriched-doc>"
  [[ -r "$native" ]]   || die "native plan not readable: $native"
  [[ -r "$enriched" ]] || die "enriched doc not readable: $enriched"

  echo "============================================================"
  echo " Backfilled plan: your approved plan + the sections target's"
  echo " gates require. Your original text is preserved verbatim."
  echo "============================================================"
  echo ""
  echo "--- YOUR APPROVED PLAN (verbatim) -------------------------"
  sed 's/^/  | /' "$native"
  echo ""
  echo "--- ADDED BY BACKFILL -------------------------------------"
  # Print only the sections the backfill owns, so the addition is visible.
  # awk: print from a ## Failure Modes / ## Acceptance Criteria / ## Execution
  # Strategy / ## File Ownership Map heading until the next top-level heading
  # that is NOT one of those owned sections.
  awk '
    function is_added(h) {
      return (h=="## Failure Modes" || h=="## Acceptance Criteria" \
           || h=="## Execution Strategy" || h=="## File Ownership Map" \
           || h=="## Patterns to Reuse")
    }
    /^## / {
      hdr=$0; sub(/[[:space:]]+$/,"",hdr)
      show=is_added(hdr)
    }
    show { print "  + " $0 }
  ' "$enriched"
  echo ""
  echo "-----------------------------------------------------------"
}

main() {
  local sub="${1:-}"
  [[ -n "$sub" ]] || die "usage: backfill-plan.sh <skeleton|check-sections|render-diff> ..."
  shift
  case "$sub" in
    skeleton)        cmd_skeleton "$@" ;;
    check-sections)  cmd_check_sections "$@" ;;
    render-diff)     cmd_render_diff "$@" ;;
    *) die "unknown subcommand: $sub" ;;
  esac
}

main "$@"
