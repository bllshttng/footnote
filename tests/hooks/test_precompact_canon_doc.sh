#!/usr/bin/env bash
# test_precompact_canon_doc.sh
#
# Tests for hooks/precompact-canon-doc.sh: the PreCompact
# mechanical backstop that writes/refreshes a session's canon handoff doc.
#
# Verifies: always exits 0 (never blocks compaction); writes the mechanical
# section structure; preserves session-written judgment across re-fires;
# degrades to omitted sections (no PR section when gh is absent); treats a
# manual /compact <path>.md as the doc target but does NOT treat prose as one.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/precompact-canon-doc.sh"

[[ -f "$HOOK" ]] || { echo "FAIL: hook not found at $HOOK" >&2; exit 1; }
export CLAUDE_PLUGIN_ROOT="$REPO_ROOT"

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t canon-doc-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT
SID="c35abbca-bd2d-4407-8365-cf468baa7eea"
DOC="$TMP/canon.md"

# Feed the hook a JSON event on stdin ($1) with the given env on the command line.
run_hook() {
  local input="$1"; shift
  printf '%s' "$input" | env "$@" CLAUDE_CODE_SESSION_ID="$SID" bash "$HOOK"
}

echo "== precompact-canon-doc.sh =="

# ---------------------------------------------------------------------------
# 1. Writes the doc structure and always exits 0.
# ---------------------------------------------------------------------------
OUT="$(run_hook "{\"trigger\":\"manual\",\"custom_instructions\":\"$DOC\"}" >/dev/null 2>&1; echo $?)"
if [[ "$OUT" == "0" ]]; then pass "exit 0 on manual compact"; else fail "expected exit 0, got $OUT"; fi
if [[ -f "$DOC" ]]; then pass "doc written to custom_instructions path"; else fail "doc not written"; fi
if grep -q "<!-- fno:auto -->" "$DOC" && grep -q "<!-- /fno:auto -->" "$DOC"; then
  pass "auto block fenced"
else
  fail "auto block markers missing"
fi
if grep -q "## Identity (auto)" "$DOC" && grep -q "## Merge order and why (session)" "$DOC" && grep -q "## Open decisions awaiting the operator (session)" "$DOC"; then
  pass "mechanical + session section headings present"
else
  fail "section headings missing"
fi
if grep -q "Session id (authoritative): \`$SID\`" "$DOC"; then
  pass "full session id recorded as authoritative key"
else
  fail "full session id not recorded"
fi

# ---------------------------------------------------------------------------
# 2. No session id anywhere -> exit 0, no stdout (nothing to point at).
# Unset every session-id env var the hook's fallback chain reads, or a leaked
# codex-companion id resolves a SID and the hook writes a real doc.
# ---------------------------------------------------------------------------
NO_SID_OUT="$(printf '{"trigger":"auto"}' \
  | env -u CLAUDE_CODE_SESSION_ID -u CODEX_COMPANION_SESSION_ID -u CODEX_SESSION_ID \
    bash "$HOOK" 2>/dev/null)"
NO_SID_RC=$?
if [[ "$NO_SID_RC" == "0" && -z "$NO_SID_OUT" ]]; then
  pass "no session id -> exit 0, no stdout"
else
  fail "no-sid case: rc=$NO_SID_RC stdout_len=${#NO_SID_OUT}"
fi

# ---------------------------------------------------------------------------
# 3. Preservation: session-written judgment survives a re-fire.
# ---------------------------------------------------------------------------
# Plant distinct content into both session blocks.
python3 - "$DOC" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
s = s.replace(
    "_Merge order and the reason for it. Nothing external knows this; the session fills it at full context._",
    "Merge #784 before #782.\nSENTINEL_MERGE_7",
)
s = s.replace(
    "_Open decisions awaiting the operator. Nothing external knows this; the session fills it at full context._",
    "dependency ordering question\nSENTINEL_DEC_7",
)
open(p, "w").write(s)
PY
run_hook "{\"trigger\":\"manual\",\"custom_instructions\":\"$DOC\"}" >/dev/null 2>&1
if grep -q "SENTINEL_MERGE_7" "$DOC" && grep -q "SENTINEL_DEC_7" "$DOC"; then
  pass "session judgment preserved across re-fire"
else
  fail "session judgment was clobbered by re-fire"
fi
# Auto block must still have refreshed (timestamp line present).
if grep -q "refreshed" "$DOC"; then
  pass "auto block refreshed alongside preserved judgment"
else
  fail "auto block did not refresh"
fi

# ---------------------------------------------------------------------------
# 4. PR section omitted when gh is absent (degrade, never a failed hook).
# ---------------------------------------------------------------------------
rm -f "$DOC"
printf '{"trigger":"manual","custom_instructions":"%s"}' "$DOC" \
  | env CLAUDE_CODE_SESSION_ID="$SID" PATH="/usr/bin:/bin" bash "$HOOK" >/dev/null 2>&1
PR_RC=$?
if [[ "$PR_RC" == "0" ]]; then
  pass "exit 0 with restricted PATH (no gh)"
else
  fail "restricted-PATH run exited $PR_RC"
fi
if [[ -f "$DOC" ]] && ! grep -q "## Open PRs (auto)" "$DOC"; then
  pass "PR section omitted when gh absent"
else
  fail "PR section present or doc missing under restricted PATH"
fi

# ---------------------------------------------------------------------------
# 5. Prose custom_instructions is NOT treated as a doc path.
# ---------------------------------------------------------------------------
PROSE_OUT="$(printf '{"trigger":"manual","custom_instructions":"focus on the auth module"}' \
  | env -u CLAUDE_CODE_SESSION_ID CLAUDE_CODE_SESSION_ID="$SID" PATH="/usr/bin:/bin" bash "$HOOK" 2>/dev/null)"
PROSE_RC=$?
# Prose is not a .md path -> falls back to `fno paths handoff`, which is absent
# on this restricted PATH -> DOC_PATH empty -> emit nothing, exit 0.
if [[ "$PROSE_RC" == "0" && -z "$PROSE_OUT" ]]; then
  pass "prose custom_instructions not treated as a path (degrades clean)"
else
  fail "prose-as-path case: rc=$PROSE_RC stdout_len=${#PROSE_OUT}"
fi

# ---------------------------------------------------------------------------
# 5b. Prose that HAPPENS to end in .md is not mis-classified as a doc path.
#     Regression guard: a bare *.md suffix test would write a junk file named
#     after the whole prose string. Requires the path anchor / existing-file
#     check. Run from a clean cwd so a stray junk file is detectable.
# ---------------------------------------------------------------------------
JUNK_DIR="$(mktemp -d -t canon-junk-XXXXXX)"
PROSE_MD_OUT="$(
  cd "$JUNK_DIR" \
  && printf '{"trigger":"manual","custom_instructions":"remember to update README.md"}' \
  | env -u CLAUDE_CODE_SESSION_ID CLAUDE_CODE_SESSION_ID="$SID" PATH="/usr/bin:/bin" bash "$HOOK" 2>/dev/null
)"
PROSE_MD_RC=$?
JUNK_CREATED="$(ls -A "$JUNK_DIR" 2>/dev/null)"
rm -rf "$JUNK_DIR"
if [[ "$PROSE_MD_RC" == "0" && -z "$PROSE_MD_OUT" && -z "$JUNK_CREATED" ]]; then
  pass "prose ending in .md not treated as a path (no junk file)"
else
  fail "prose-.md case: rc=$PROSE_MD_RC out=${#PROSE_MD_OUT} junk=[${JUNK_CREATED}]"
fi

# ---------------------------------------------------------------------------
# 6. Creates a doc in a not-yet-existing nested dir (mkdir -p the parent).
# ---------------------------------------------------------------------------
NESTED="$TMP/nested/deep/canon.md"
run_hook "{\"trigger\":\"manual\",\"custom_instructions\":\"$NESTED\"}" >/dev/null 2>&1
if [[ -f "$NESTED" ]]; then
  pass "doc written into a freshly created nested dir"
else
  fail "nested-dir doc not written (parent not created)"
fi

echo
echo "results: PASS=$PASS FAIL=$FAIL"
[[ "$FAIL" == 0 ]]
