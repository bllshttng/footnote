#!/usr/bin/env bash
# scripts/ci/check-session-id-provenance-rule.sh
#
# One registry lookup, two docstrings, opposite readings: that pair is how
# the self-as-competitor defect shipped, and prose between the two sites
# cannot be shared by import (the L1-to-L5 boundary gate refuses that edge).
# The canonical sentence lives ONCE, under a marker pair in
# docs/architecture/coordination.md, and this gate asserts both docstrings
# carry it verbatim. It is a shared SENTENCE, never a shared import.
#
# Modes:
#   (default)    inspect the COMMITTED bytes of every path
#   --worktree   inspect the working-tree bytes
#
# Comparing HEAD by default closes the ordering hazard a builder creates: a
# job that regenerates or reformats first would resync the WORKTREE and
# launder a hand edit that is still committed. HEAD is order-independent.
#
# Zero sites carrying the sentence is exit 2, never a silent green: a rename
# that moved or reworded both docstrings must fail loudly, not pass on an
# empty scan.
#
# Exit codes: 0 both sites carry the sentence, 1 diverged, 2 misuse (a file
# is missing, the marker is malformed, or zero sites were found).
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/gate-fresh-common.sh"
gate_parse_mode "$@"
gate_resolve_repo_root
MODE="$GATE_MODE"

if [[ "$MODE" == "write" ]]; then
  echo "ERROR: --write is not supported: the canonical sentence is prose and is not auto-fixable" >&2
  exit 2
fi

COORD_REL="docs/architecture/coordination.md"
SITE_RELS=(
  "cli/src/fno/agents/registry.py"
  "cli/src/fno/claims/self_identity.py"
)

TMPDIR_GATE="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_GATE"' EXIT

resolve_bytes() {
  # $1 repo-relative path, $2 temp destination. Prints the path whose bytes
  # to inspect: the HEAD blob in committed mode, else the worktree file.
  local rel="$1" dest="$2"
  if [[ "$MODE" == "worktree" ]]; then
    echo "$REPO_ROOT/$rel"
    return 0
  fi
  if gate_committed_blob "$rel" "$dest"; then
    echo "$dest"
  else
    echo "NOTE: no committed blob for $rel; comparing its worktree bytes instead" >&2
    echo "$REPO_ROOT/$rel"
  fi
}

COORD="$(resolve_bytes "$COORD_REL" "$TMPDIR_GATE/coord")"
if [[ ! -f "$COORD" ]]; then
  echo "ERROR: canonical sentence file missing at $COORD_REL" >&2
  exit 2
fi

# The sentence is the single non-blank line between the marker pair. Anything
# else is marker misuse and must refuse rather than guess.
SENTENCE="$(awk '
  /session-id-provenance-rule:begin/ {inside = 1; next}
  /session-id-provenance-rule:end/   {inside = 0}
  inside
' "$COORD" | sed '/^[[:space:]]*$/d')"
if [[ -z "$SENTENCE" || "$SENTENCE" == *$'\n'* ]]; then
  echo "ERROR: expected exactly one non-blank line between the session-id-provenance-rule markers in $COORD_REL" >&2
  exit 2
fi

failed=0
sites_found=0
i=0
for rel in "${SITE_RELS[@]}"; do
  SITE="$(resolve_bytes "$rel" "$TMPDIR_GATE/site-$i")"
  if [[ ! -f "$SITE" ]]; then
    echo "ERROR: provenance-rule site missing at $rel" >&2
    exit 2
  fi
  if grep -F -q -- "$SENTENCE" "$SITE"; then
    sites_found=$((sites_found + 1))
  else
    failed=1
  fi
  i=$((i + 1))
done

if [[ "$sites_found" -eq 0 ]]; then
  echo "ERROR: ZERO provenance-rule sites carry the canonical sentence. A rename" >&2
  echo "that silenced both docstrings must read as a refusal, never as a pass:" >&2
  echo "  sentence: $SENTENCE" >&2
  echo "  sites:    ${SITE_RELS[*]}" >&2
  exit 2
fi

if [[ "$failed" -ne 0 ]]; then
  echo "ERROR: the two session-id docstrings disagree about the provenance rule:" >&2
  for rel in "${SITE_RELS[@]}"; do
    echo "  site: $rel" >&2
  done
  echo "  canonical sentence ($COORD_REL): $SENTENCE" >&2
  echo "" >&2
  echo "Both docstrings must carry the sentence verbatim. They reconcile only on" >&2
  echo "the provenance of the id under test; a one-sided edit reads as a rule change." >&2
  exit 1
fi

echo "session-id provenance rule: both sites carry the canonical sentence ($MODE bytes)"
