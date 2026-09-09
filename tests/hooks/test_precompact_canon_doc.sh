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
  printf '%s' "$input" | env "$@" CODEX_THREAD_ID="foreign-codex-thread" \
    CLAUDE_CODE_SESSION_ID="$SID" bash "$HOOK"
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
# A leaked Codex marker must not identify a Claude hook. Unset the Claude marker
# while leaving a foreign Codex marker present to prove harness-local selection.
# ---------------------------------------------------------------------------
NO_SID_OUT="$(printf '{"trigger":"auto"}' \
  | env -u CLAUDE_CODE_SESSION_ID CODEX_THREAD_ID="foreign-codex-thread" \
    bash "$HOOK" 2>/dev/null)"
NO_SID_RC=$?
if [[ "$NO_SID_RC" == "0" && -z "$NO_SID_OUT" ]]; then
  pass "no session id -> exit 0, no stdout"
else
  fail "no-sid case: rc=$NO_SID_RC stdout_len=${#NO_SID_OUT}"
fi

# ---------------------------------------------------------------------------
# 2b. Codex must use its own plugin root and thread id even when a parent
# Claude marker and plugin root leak into the environment.
# ---------------------------------------------------------------------------
CODEX_DOC="$TMP/codex-canon.md"
printf '{"trigger":"manual","custom_instructions":"%s"}' "$CODEX_DOC" \
  | env FNO_PLATFORM=codex PLUGIN_ROOT="$REPO_ROOT" \
    CLAUDE_PLUGIN_ROOT="$TMP/foreign-claude-plugin" \
    CLAUDE_CODE_SESSION_ID="foreign-claude-session" CODEX_THREAD_ID="$SID" \
    bash "$HOOK" >/dev/null 2>&1
if [[ -f "$CODEX_DOC" ]] && grep -q "Session id (authoritative): \`$SID\`" "$CODEX_DOC"; then
  pass "codex ignores leaked Claude root and session markers"
else
  fail "codex root or session selection followed leaked Claude state"
fi

# ---------------------------------------------------------------------------
# 3. Preservation: session-written judgment survives a re-fire.
# ---------------------------------------------------------------------------
# Plant distinct content into both session blocks.
python3 - "$DOC" <<'PY'
import re
import sys
p = sys.argv[1]
s = open(p).read()
# Match the placeholder by its OPENING words, not the whole sentence: pinning
# the full text made a reworded default a silent no-op, so the sentinels never
# landed and this case failed as a clobber that never happened.
s, n1 = re.subn(
    r"_Merge order and the reason for it\.[^\n]*_",
    "Merge #784 before #782.\nSENTINEL_MERGE_7",
    s,
)
s, n2 = re.subn(
    r"_Open decisions awaiting the operator\.[^\n]*_",
    "dependency ordering question\nSENTINEL_DEC_7",
    s,
)
assert n1 == 1 and n2 == 1, f"placeholders not found: merge={n1} decisions={n2}"
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
# 3b. A session-authored doc carrying none of the hook's markers is preserved.
# The preserve helper above only reads content between `fno:session` markers,
# so a doc the session wrote by hand - the shape a manual `/compact <path>`
# supplies - was truncated whole. Content above the hook's own title line is
# kept verbatim, and stays kept across the re-fire that follows.
# ---------------------------------------------------------------------------
HAND="$TMP/hand-written.md"
cat > "$HAND" <<'MD'
---
created: 2026-08-13T13:39
---
# Session brief

SENTINEL_HAND_9 the ordering constraint and why it is the hard part.
MD
run_hook "{\"trigger\":\"manual\",\"custom_instructions\":\"$HAND\"}" >/dev/null 2>&1
if grep -q "SENTINEL_HAND_9" "$HAND" && grep -q "created: 2026-08-13T13:39" "$HAND"; then
  pass "hand-written doc preserved when the hook enriches it"
else
  fail "hand-written doc clobbered by the hook"
fi
if grep -q "<!-- fno:auto -->" "$HAND"; then
  pass "auto block added below the hand-written body"
else
  fail "auto block not added to hand-written doc"
fi
run_hook "{\"trigger\":\"manual\",\"custom_instructions\":\"$HAND\"}" >/dev/null 2>&1
if [[ "$(grep -c "SENTINEL_HAND_9" "$HAND")" == "1" ]]; then
  pass "hand-written body survives the re-fire exactly once"
else
  fail "hand-written body lost or duplicated on re-fire"
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
# Prose is not a .md path -> falls back to `fno config paths handoff`, which is absent
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

# ---------------------------------------------------------------------------
# 7. AC2-EDGE: an uncrowned session's doc carries no King block at all.
# Assert a positive marker too (crown: none), not absence alone - an absence
# also fires if crown classification or auto-block generation never ran.
# ---------------------------------------------------------------------------
if grep -q "crown: none" "$DOC" && ! grep -q "## King:" "$DOC"; then
  pass "uncrowned doc carries no King block (AC2-EDGE)"
else
  fail "uncrowned doc unexpectedly carries a King block, or the auto block never ran"
fi

# ---------------------------------------------------------------------------
# 8. AC1-HP: a crowned session's doc gains the King block and both new
# session headings. A fake `fno` on PATH stands in for the registry and the
# epic-status read so the fixture never touches the real graph.
# ---------------------------------------------------------------------------
FAKE_BIN="$(mktemp -d -t canon-fake-fno-XXXXXX)"
cat > "$FAKE_BIN/fno" <<'FAKE'
#!/usr/bin/env bash
case "$*" in
  *"agents registry-json"*)
    echo '[{"session_id":"c35abbca-bd2d-4407-8365-cf468baa7eea","crown_level":2,"crown_scope":"x-9e1e-fixture","name":"king-fixture"}]'
    ;;
  *"backlog epic status x-9e1e-fixture"*)
    echo '{"children":[{"id":"x-aaaa","status":"ready","slug":"a"},{"id":"x-bbbb","status":"in_progress","slug":"b"}]}'
    ;;
  *"do pr list"*)
    echo '[]'
    ;;
  *)
    exit 1
    ;;
esac
FAKE
chmod +x "$FAKE_BIN/fno"
trap 'rm -rf "$TMP" "$FAKE_BIN"' EXIT

CROWNED_DOC="$TMP/crowned-canon.md"
printf '{"trigger":"manual","custom_instructions":"%s"}' "$CROWNED_DOC" \
  | env PATH="$FAKE_BIN:$PATH" CLAUDE_CODE_SESSION_ID="$SID" bash "$HOOK" >/dev/null 2>&1
if grep -q "## King: nodes under purview (auto)" "$CROWNED_DOC" \
  && grep -q "level 2 over x-9e1e-fixture" "$CROWNED_DOC" \
  && grep -q "x-aaaa \[ready\] a" "$CROWNED_DOC" \
  && grep -q "x-bbbb \[in_progress\] b" "$CROWNED_DOC"; then
  pass "crowned doc gains the King block naming level, scope, and children"
else
  fail "crowned doc missing King block content"
fi
if grep -q "## Gaps and open thinking (session)" "$CROWNED_DOC" \
  && grep -q "## Workarounds in force (session)" "$CROWNED_DOC"; then
  pass "crowned doc gains the two new session headings"
else
  fail "crowned doc missing the new session headings"
fi

# ---------------------------------------------------------------------------
# 9. AC1-HP portfolio case: a level-2 crown over TWO epics (a comma-joined
# scope, fno.agents.crown.canonical_scope's stored shape) sees BOTH epics'
# children, not just the first.
# ---------------------------------------------------------------------------
PORTFOLIO_BIN="$(mktemp -d -t canon-fake-fno-portfolio-XXXXXX)"
cat > "$PORTFOLIO_BIN/fno" <<'FAKE'
#!/usr/bin/env bash
case "$*" in
  *"agents registry-json"*)
    echo '[{"session_id":"c35abbca-bd2d-4407-8365-cf468baa7eea","crown_level":2,"crown_scope":"x-epic-a,x-epic-b","name":"king-fixture"}]'
    ;;
  *"backlog epic status x-epic-a"*)
    echo '{"children":[{"id":"x-aaaa","status":"ready","slug":"a"}]}'
    ;;
  *"backlog epic status x-epic-b"*)
    echo '{"children":[{"id":"x-bbbb","status":"in_progress","slug":"b"}]}'
    ;;
  *"do pr list"*)
    echo '[]'
    ;;
  *)
    exit 1
    ;;
esac
FAKE
chmod +x "$PORTFOLIO_BIN/fno"
trap 'rm -rf "$TMP" "$FAKE_BIN" "$PORTFOLIO_BIN"' EXIT

PORTFOLIO_DOC="$TMP/portfolio-canon.md"
printf '{"trigger":"manual","custom_instructions":"%s"}' "$PORTFOLIO_DOC" \
  | env PATH="$PORTFOLIO_BIN:$PATH" CLAUDE_CODE_SESSION_ID="$SID" bash "$HOOK" >/dev/null 2>&1
if grep -q "x-aaaa \[ready\] a" "$PORTFOLIO_DOC" && grep -q "x-bbbb \[in_progress\] b" "$PORTFOLIO_DOC"; then
  pass "portfolio crown (comma-joined scope): both epics' children appear"
else
  fail "portfolio crown: missing children from one or both epics"
fi

# ---------------------------------------------------------------------------
# 10. A king hand-writes ONLY the two crown headings (the shape context-nudge.sh
# now tells a king to write on a FIRST compaction, when the doc - and its
# headings 1/2 - do not exist yet). The hook must bind each by heading text,
# not by ordinal position, so both survive the fire that follows.
# ---------------------------------------------------------------------------
HANDWRITTEN_DOC="$TMP/handwritten-canon.md"
cat > "$HANDWRITTEN_DOC" <<'DOC'
## Gaps and open thinking (session)
<!-- fno:session -->
Unsure whether the blocked_child queue drains fairly under contention.
<!-- /fno:session -->

## Workarounds in force (session)
<!-- fno:session -->
Routing around the stale-epic-status cache by re-querying every 5s.
<!-- /fno:session -->
DOC
printf '{"trigger":"manual","custom_instructions":"%s"}' "$HANDWRITTEN_DOC" \
  | env PATH="$FAKE_BIN:$PATH" CLAUDE_CODE_SESSION_ID="$SID" bash "$HOOK" >/dev/null 2>&1
if grep -q "Unsure whether the blocked_child queue drains fairly" "$HANDWRITTEN_DOC" \
  && grep -q "Routing around the stale-epic-status cache" "$HANDWRITTEN_DOC" \
  && grep -q "_Merge order and the reason for it" "$HANDWRITTEN_DOC"; then
  pass "hand-written crown-only headings bind by label, not ordinal position"
else
  fail "hand-written crown headings lost or misplaced by the refire"
fi

echo
echo "results: PASS=$PASS FAIL=$FAIL"
[[ "$FAIL" == 0 ]]
