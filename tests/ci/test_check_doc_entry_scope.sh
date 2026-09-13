#!/usr/bin/env bash
# tests/ci/test_check_doc_entry_scope.sh
#
# Exercises scripts/ci/check-doc-entry-scope.sh against a fixture AGENTS.md
# and three pages: one good, one with no scope section, one whose section
# lacks a `Not for:` line. Every case asserts the exit code AND a line only
# that outcome prints, so a gate that skipped a path cannot pass by printing
# nothing.
#
# Run: bash tests/ci/test_check_doc_entry_scope.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$(cd "${SCRIPT_DIR}/../.." && pwd)/scripts/ci/check-doc-entry-scope.sh"
[[ -f "$GATE" ]] || { echo "gate not found at $GATE" >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

PASS=0; FAIL=0

# check <label> <want_exit> <marker>
check() {
  local label="$1" want="$2" marker="$3" out got
  out="$(bash "$GATE" "$TMP/AGENTS.md" 2>&1)"; got=$?
  if [[ "$got" -eq "$want" && "$out" == *"$marker"* ]]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s\n  want exit %s with: %s\n  got exit %s:\n%s\n' "$label" "$want" "$marker" "$got" "$out"
  fi
}

fixture_index() {
  cat > "$TMP/AGENTS.md" <<'EOF'
# AGENTS.md

## Deep-dive docs

Backlog: [usage](docs/usage.md), [ordering](docs/ordering.md), [triage](docs/triage.md)
EOF
}

write_good() {
  cat > "$TMP/docs/usage.md" <<'EOF'
# Usage

## Is this page for you?

You edit the board daily. Misreading the rank rules corrupts your work order.

Not for: dispatch failures, which the board never causes; see the loop doc.
EOF
}

write_no_section() {
  cat > "$TMP/docs/ordering.md" <<'EOF'
# Ordering

## Mental model

The board is derived.
EOF
}

write_no_notfor() {
  cat > "$TMP/docs/triage.md" <<'EOF'
# Triage

## Is this page for you?

You triage intake. Misreading the columns stalls the queue.
EOF
}

mkdir -p "$TMP/docs"

# --- red: both defective pages are named with their reasons -------------------
fixture_index; write_good; write_no_section; write_no_notfor
out="$(bash "$GATE" "$TMP/AGENTS.md" 2>&1)"; got=$?
if [[ "$got" -eq 1 && "$out" == *'ordering.md'* && "$out" == *'triage.md'* && "$out" != *'usage.md'* ]]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1))
  printf 'FAIL: both defective pages named, the good one absent\n  want exit 1 naming ordering.md and triage.md, not usage.md\n  got exit %s:\n%s\n' "$got" "$out"
fi

# --- green: fixing both pages passes with the page count ---------------------
write_good; cat > "$TMP/docs/ordering.md" <<'EOF'
# Ordering

## Is this page for you?

You ask why one card sits above another. Misreading it wrecks the dispatch order.

Not for: why advance did not dispatch a card; that is the loop doc.
EOF
cat > "$TMP/docs/triage.md" <<'EOF'
# Triage

## Is this page for you?

You triage intake. Misreading the columns stalls the queue.

Not for: board rank, which ordering.md owns.
EOF
check 'fixed fixture passes with the page count' 0 'check-doc-entry-scope: 3 pages checked'

# --- a missing page is named --------------------------------------------------
fixture_index
cat > "$TMP/AGENTS.md" <<'EOF'
# AGENTS.md

## Deep-dive docs

Ops: [ghost](docs/ghost.md)
EOF
check 'a linked but missing page is named' 1 "'docs/ghost.md' is linked from the Deep-dive docs index but the file is missing"

# --- a zero-link index is refused, never a silent pass ------------------------
cat > "$TMP/AGENTS.md" <<'EOF'
# AGENTS.md

## Deep-dive docs

Nothing here yet.
EOF
check 'a zero-link index is refused' 1 'yielded zero docs/ links'

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
