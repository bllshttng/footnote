#!/usr/bin/env bash
# test_canon_doc_marker.sh
#
# Direct tests of scripts/lib/canon-doc-marker.sh: the one reader for the
# canon doc's fenced marker blocks. Verifies: a closed block extracts exactly;
# a missing closing marker is content (captured to the next heading or EOF,
# never a parse error); an absent marker or unreadable file exits 1 with no
# output; a found-but-empty block exits 0 with no output; and the placeholder
# helpers classify the seed line and real user text apart.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LIB="$REPO_ROOT/scripts/lib/canon-doc-marker.sh"

[[ -f "$LIB" ]] || { echo "FAIL: lib not found at $LIB" >&2; exit 1; }
# shellcheck source=../../scripts/lib/canon-doc-marker.sh
source "$LIB"

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t canon-marker-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

echo "== canon-doc-marker.sh =="

# ---------------------------------------------------------------------------
# 1. Closed block: exact content, markers excluded, exit 0.
# ---------------------------------------------------------------------------
DOC="$TMP/closed.md"
printf 'before\n<!-- fno:user -->\nUSER LINE 1\nUSER LINE 2\n<!-- /fno:user -->\nafter\n' > "$DOC"
OUT="$(canon_doc_extract_marker "$DOC" user)"
RC=$?
if [[ "$RC" == "0" && "$OUT" == "USER LINE 1
USER LINE 2" ]]; then
  pass "closed block extracts content exactly"
else
  fail "closed block rc=$RC out=[$OUT]"
fi

# ---------------------------------------------------------------------------
# 2. Missing closing marker, next heading bounds the capture: CONTENT, not an
#    error. Exit 0 with the text intact is the contract the write-through's
#    self-heal depends on.
# ---------------------------------------------------------------------------
BROKEN="$TMP/broken.md"
printf '<!-- fno:user -->\nPARTIAL EDIT TEXT\n## Next section\nunrelated\n' > "$BROKEN"
OUT="$(canon_doc_extract_marker "$BROKEN" user)"
RC=$?
if [[ "$RC" == "0" && "$OUT" == "PARTIAL EDIT TEXT" ]]; then
  pass "missing closing marker: captured to the next heading, exit 0"
else
  fail "missing closing (heading) rc=$RC out=[$OUT]"
fi

# ---------------------------------------------------------------------------
# 3. Missing closing marker at end of file: capture runs to EOF.
# ---------------------------------------------------------------------------
BROKEN_EOF="$TMP/broken-eof.md"
printf 'x\n<!-- fno:user -->\nEOF BOUND TEXT\n' > "$BROKEN_EOF"
OUT="$(canon_doc_extract_marker "$BROKEN_EOF" user)"
RC=$?
if [[ "$RC" == "0" && "$OUT" == "EOF BOUND TEXT" ]]; then
  pass "missing closing marker at EOF: captured to EOF, exit 0"
else
  fail "missing closing (EOF) rc=$RC out=[$OUT]"
fi

# ---------------------------------------------------------------------------
# 4. Marker absent: exit 1, no output (the caller seeds its placeholder).
# ---------------------------------------------------------------------------
OUT="$(canon_doc_extract_marker "$DOC" absent)"
RC=$?
if [[ "$RC" == "1" && -z "$OUT" ]]; then
  pass "absent marker: exit 1, no output"
else
  fail "absent marker rc=$RC out=[$OUT]"
fi

# ---------------------------------------------------------------------------
# 5. Unreadable/missing file: exit 1, no output, never an error spew.
# ---------------------------------------------------------------------------
OUT="$(canon_doc_extract_marker "$TMP/does-not-exist.md" user 2>/dev/null)"
RC=$?
if [[ "$RC" == "1" && -z "$OUT" ]]; then
  pass "missing file: exit 1, no output"
else
  fail "missing file rc=$RC out=[$OUT]"
fi

# ---------------------------------------------------------------------------
# 6. Found-but-empty block: exit 0 (marker exists), no output. The write-
#    through reads the exit status so an emptied block stays emptied - the
#    machine never re-seeds content into a block the user cleared.
# ---------------------------------------------------------------------------
EMPTY="$TMP/empty.md"
printf '<!-- fno:user -->\n<!-- /fno:user -->\n' > "$EMPTY"
OUT="$(canon_doc_extract_marker "$EMPTY" user)"
RC=$?
if [[ "$RC" == "0" && -z "$OUT" ]]; then
  pass "found-but-empty block: exit 0, no output"
else
  fail "empty block rc=$RC out=[$OUT]"
fi

# ---------------------------------------------------------------------------
# 7. Only the exact marker name opens the capture: fno:user must not match a
#    block fenced for a different marker sharing the prefix, and text outside
#    the block never leaks in.
# ---------------------------------------------------------------------------
NAMESPACED="$TMP/namespaced.md"
printf '<!-- fno:users -->\nWRONG BLOCK\n<!-- /fno:users -->\n<!-- fno:user -->\nRIGHT BLOCK\n<!-- /fno:user -->\n' > "$NAMESPACED"
OUT="$(canon_doc_extract_marker "$NAMESPACED" user)"
RC=$?
if [[ "$RC" == "0" && "$OUT" == "RIGHT BLOCK" ]]; then
  pass "marker name matched exactly, no prefix bleed"
else
  fail "namespaced marker rc=$RC out=[$OUT]"
fi

# ---------------------------------------------------------------------------
# 8. Placeholder helpers: the seed line classifies as placeholder, user text
#    and placeholder-plus-user-text do not.
# ---------------------------------------------------------------------------
if canon_doc_is_placeholder "$(canon_doc_user_placeholder)"; then
  pass "seed line classifies as placeholder"
else
  fail "seed line not recognized as placeholder"
fi
if ! canon_doc_is_placeholder "an actual instruction from the user"; then
  pass "user text does not classify as placeholder"
else
  fail "user text misclassified as placeholder"
fi
if ! canon_doc_is_placeholder "$(canon_doc_user_placeholder)
and a real instruction"; then
  pass "placeholder plus user text does not classify as placeholder"
else
  fail "placeholder+text misclassified as placeholder"
fi

echo
echo "results: PASS=$PASS FAIL=$FAIL"
[[ "$FAIL" == 0 ]]
