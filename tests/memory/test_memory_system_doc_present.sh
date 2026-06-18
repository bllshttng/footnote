#!/usr/bin/env bash
# Tests that docs/architecture/memory-system.md exists with required sections.
# AC2.1-FR: docs accurate.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DOC="$REPO_ROOT/docs/architecture/memory-system.md"
PASS=0
FAIL=0

pass() { echo "PASS: $1"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $1"; FAIL=$((FAIL+1)); }

# AC2.1-FR-1: file exists
if [[ -f "$DOC" ]]; then
    pass "docs/architecture/memory-system.md exists"
else
    fail "docs/architecture/memory-system.md MISSING"
    echo "Results: $PASS passed, $FAIL failed"
    exit 1
fi

# AC2.1-FR-2: contains "Two-checkpoint memory pass" section
if grep -q "Two-checkpoint memory pass" "$DOC"; then
    pass "contains 'Two-checkpoint memory pass'"
else
    fail "missing 'Two-checkpoint memory pass'"
fi

# AC2.1-FR-3: contains "Pre-promise pass" section
if grep -q "Pre-promise pass" "$DOC"; then
    pass "contains 'Pre-promise pass'"
else
    fail "missing 'Pre-promise pass'"
fi

# AC2.1-FR-4: contains "Post-merge pass" section
if grep -q "Post-merge pass" "$DOC"; then
    pass "contains 'Post-merge pass'"
else
    fail "missing 'Post-merge pass'"
fi

# AC2.1-FR-5: mentions deprecated or deprecation
if grep -qi "deprecat" "$DOC"; then
    pass "contains deprecation reference"
else
    fail "missing deprecation reference"
fi

# AC2.1-FR-6: references write-memory-entry.sh
if grep -q "write-memory-entry.sh" "$DOC"; then
    pass "references write-memory-entry.sh"
else
    fail "missing write-memory-entry.sh reference"
fi

# AC2.1-FR-7: does NOT cite distill-session.sh as the active path
# (it can mention it as deprecated, but must not claim it's the active path)
if grep -q "distill-session.sh" "$DOC"; then
    # Acceptable only if paired with "deprecated" or "stub" nearby
    if grep -B2 -A2 "distill-session.sh" "$DOC" | grep -qi "deprecat\|stub"; then
        pass "distill-session.sh mentioned only in deprecated context"
    else
        fail "distill-session.sh mentioned as active path (must be deprecated context only)"
    fi
else
    pass "distill-session.sh not cited as active path"
fi

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
