#!/usr/bin/env bash
# test_king_postcompact_reinject.sh
#
# Unit tests for hooks/king-postcompact-reinject.sh: the post-compaction
# re-injection of the king's operating brief. Verifies: the crowned claude case
# delivers the brief through hookSpecificOutput.additionalContext; the crowned
# codex case delivers through systemMessage; uncrowned rows, unknown sessions,
# non-compact sources, and a missing fno all degrade to empty output with exit 0;
# and the brief stays inside its byte budget (it is paid on every compaction).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
KING="$REPO_ROOT/hooks/king-postcompact-reinject.sh"
BRIEF="$REPO_ROOT/skills/reign/references/postcompact-brief.md"
# Keep the cap fixed: the brief must fit without changing the budget.
BRIEF_MAX_BYTES=1800

[[ -f "$KING" ]] || { echo "FAIL: king hook not found at $KING" >&2; exit 1; }
export CLAUDE_PLUGIN_ROOT="$REPO_ROOT"

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t king-reinject-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# Stub `fno` answering `agents registry-json` from a per-case fixture file, and
# `agents king faq list --scope X` from a second fixture keyed by scope, so no
# real registry, daemon, or FAQ store is involved. $KING_REG_FIXTURE and
# $KING_FAQ_FIXTURE select the payloads; the FAQ fixture is empty (no output)
# unless a test overwrites it.
mkdir -p "$TMP/bin"
cat > "$TMP/bin/fno" <<'STUB'
#!/usr/bin/env bash
if [ "$1" = "agents" ] && [ "$2" = "registry-json" ]; then
  # Mirror the real Rust client: the verb refuses while the anti-recursion
  # pin is set (a leaked FNO_AGENTS_RUNTIME=python). The hook must
  # strip the pin before the read.
  if [ -n "${FNO_AGENTS_RUNTIME:-}" ]; then
    echo "no Python implementation -- this verb runs only on the 'fno-agents' Rust runtime" >&2
    exit 127
  fi
  cat "$KING_REG_FIXTURE"
elif [ "$1" = "agents" ] && [ "$2" = "king" ] && [ "$3" = "faq" ] && [ "$4" = "list" ]; then
  cat "$KING_FAQ_FIXTURE" 2>/dev/null || true
elif [ "$1" = "config" ] && [ "$2" = "paths" ] && [ "$3" = "handoff" ]; then
  cat "$KING_HANDOFF_PATH_FIXTURE" 2>/dev/null || true
elif [ "$1" = "backlog" ] && [ "$2" = "get" ]; then
  # Batch node read: the fixture answers with the real verb's shape (a JSON
  # array, misses as {"id":...,"error":"not found"}). Absent fixture = the
  # verb itself failing (empty stdout, nonzero exit).
  if [ -n "$KING_BACKLOG_FIXTURE" ] && [ -f "$KING_BACKLOG_FIXTURE" ]; then
    cat "$KING_BACKLOG_FIXTURE"
  else
    exit 1
  fi
else
  exit 1
fi
STUB
chmod +x "$TMP/bin/fno"
export PATH="$TMP/bin:$PATH"
export KING_REG_FIXTURE="$TMP/registry.json"
export KING_FAQ_FIXTURE="$TMP/faq.txt"
export KING_HANDOFF_PATH_FIXTURE="$TMP/handoff-path.txt"
: > "$KING_FAQ_FIXTURE"
: > "$KING_HANDOFF_PATH_FIXTURE"

SID="sess-king"
SID_OTHER="sess-someone-else"

# registry_fixture <row-json> - write a one-row registry around the given row.
registry_fixture() {
  printf '{"agents":[%s]}\n' "$1" > "$KING_REG_FIXTURE"
}

CROWNED_ROW='{"session_id":"'"$SID"'","harness_session_id":"full-'"$SID"'","name":"king","status":"live","crown_level":1,"crown_scope":"fno"}'
CROWNED_HARNESS_ROW='{"session_id":"short-king","harness_session_id":"'"$SID"'","name":"king","status":"live","crown_level":1,"crown_scope":"fno"}'
UNCROWNED_ROW='{"session_id":"'"$SID"'","harness_session_id":"full-'"$SID"'","name":"worker","status":"live","crown_level":null,"crown_scope":null}'

run_king() { # $1 = event JSON ; FNO_PLATFORM env selects the lane
  printf '%s' "$1" | FNO_PLATFORM="$FNO_PLATFORM" bash "$KING" 2>/dev/null
}

# 1. Crowned row, source=compact, claude lane: the brief must arrive on the
#    model-context carrier with both the crown line and the first rule.
registry_fixture "$CROWNED_ROW"
FNO_PLATFORM=claude
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID\"}")"
RC=$?
[[ $RC -eq 0 ]] && echo "$OUT" | jq -e '.hookSpecificOutput.additionalContext
    | contains("level 1 over fno") and contains("Encode, then abdicate")
      and contains("--substrate thread") and contains("glm-5.3-flash[1m]")
      and contains("status=retasked") and contains("spawn_required")
      and (contains("retier: ") | not)' >/dev/null 2>&1 \
  && pass "crowned claude: additionalContext carries crown + first rule + retask receipts" \
  || fail "crowned claude rc=$RC payload=$OUT"

# 2. Crowned row, no source field, codex lane resolved through CODEX_THREAD_ID:
#    the real codex PostCompact event carries no session_id at all, so the SID
#    must come from the env marker the registry row's harness_session_id holds.
#    systemMessage carrier, never the claude-only hookSpecificOutput key.
registry_fixture "$CROWNED_HARNESS_ROW"
FNO_PLATFORM=codex
OUT="$(printf '%s' '{}' | env -u CODEX_SESSION_ID CODEX_THREAD_ID="$SID" FNO_PLATFORM=codex \
  CODEX_PLUGIN_ROOT="$REPO_ROOT" PLUGIN_ROOT="$REPO_ROOT" CLAUDE_PLUGIN_ROOT="$TMP/foreign-claude-plugin" \
  bash "$KING" 2>/dev/null)"
RC=$?
[[ $RC -eq 0 ]] && echo "$OUT" | jq -e 'has("systemMessage") and (has("hookSpecificOutput") | not)' >/dev/null 2>&1 \
  && pass "crowned codex via CODEX_THREAD_ID: systemMessage carrier" \
  || fail "crowned codex rc=$RC payload=$OUT"

# 3. Uncrowned row (both crown fields null): nothing to re-teach, silence.
registry_fixture "$UNCROWNED_ROW"
FNO_PLATFORM=claude
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID\"}")"; RC=$?
[[ $RC -eq 0 && -z "$OUT" ]] && pass "uncrowned row: empty stdout, exit 0" \
  || fail "uncrowned row rc=$RC out=$OUT"

# 4. No row for this session id: the hook is not for this session, silence.
registry_fixture "$CROWNED_ROW"
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID_OTHER\"}")"; RC=$?
[[ $RC -eq 0 && -z "$OUT" ]] && pass "no registry row: empty stdout, exit 0" \
  || fail "no-row rc=$RC out=$OUT"

# 5. source=startup: the defensive gate must hold independent of registration.
OUT="$(run_king "{\"source\":\"startup\",\"session_id\":\"$SID\"}")"; RC=$?
[[ $RC -eq 0 && -z "$OUT" ]] && pass "source=startup: empty stdout, exit 0" \
  || fail "startup rc=$RC out=$OUT"

# 6. No fno on PATH: no registry to read, silence (never a failed compaction).
OUT="$(printf '%s' "{\"source\":\"compact\",\"session_id\":\"$SID\"}" \
  | env PATH="/usr/bin:/bin" FNO_PLATFORM=claude bash "$KING" 2>/dev/null)"; RC=$?
[[ $RC -eq 0 && -z "$OUT" ]] && pass "no fno on PATH: empty stdout, exit 0" \
  || fail "no-fno rc=$RC out=$OUT"

# 6b. AC5-HP: a leaked FNO_AGENTS_RUNTIME=python pin must not blind
#     the crown read. The stub refuses registry-json under the pin (mirroring
#     the real Rust client); the hook strips the pin before the read, so the
#     crowned reinjection still arrives and no failing-read line fires.
registry_fixture "$CROWNED_ROW"
FNO_PLATFORM=claude
PIN_ERR="$TMP/pin-err.txt"
OUT="$(printf '%s' "{\"source\":\"compact\",\"session_id\":\"$SID\"}" \
  | env FNO_PLATFORM=claude FNO_AGENTS_RUNTIME=python bash "$KING" 2>"$PIN_ERR")"; RC=$?
[[ $RC -eq 0 ]] && echo "$OUT" | jq -e '.hookSpecificOutput.additionalContext
    | contains("level 1 over fno")' >/dev/null 2>&1 \
  && ! grep -q "registry-json exited" "$PIN_ERR" \
  && pass "pinned env: crown read survives the strip, reinjection arrives (AC5-HP)" \
  || fail "pinned env rc=$RC payload=$OUT err=$(cat "$PIN_ERR" 2>/dev/null)"

# 7b. A crowned king with matching FAQ entries gets them after the static
#     brief; an empty FAQ fixture (the default, case 1 above) adds nothing.registry_fixture "$CROWNED_ROW"
printf 'Q: what do I do?\nA: reign on.\n---\n' > "$KING_FAQ_FIXTURE"
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID\"}")"
RC=$?
[[ $RC -eq 0 ]] && echo "$OUT" | jq -e '.hookSpecificOutput.additionalContext
    | contains("This crown'"'"'s FAQ") and contains("reign on.")' >/dev/null 2>&1 \
  && pass "crowned with matching FAQ entries: appended after the brief" \
  || fail "crowned+FAQ rc=$RC payload=$OUT"
: > "$KING_FAQ_FIXTURE"
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID\"}")"
RC=$?
# Positive marker required alongside the absence: a regression to empty
# output would also pass "no FAQ heading", so require the base brief too.
[[ $RC -eq 0 ]] && echo "$OUT" | grep -q "level 1 over fno" && ! echo "$OUT" | grep -q "This crown's FAQ" \
  && pass "crowned with empty FAQ fixture: no FAQ heading added" \
  || fail "crowned+empty-FAQ rc=$RC payload=$OUT"

# 7c. An oversized FAQ payload is truncated to the byte budget, never
#     reinjected whole - one detailed entry can already outgrow context.
python3 -c "print('Q: big?\nA: ' + ('x' * 6000) + '\n---')" > "$KING_FAQ_FIXTURE"
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID\"}")"
RC=$?
[[ $RC -eq 0 ]] && echo "$OUT" | grep -q "truncated at" \
  && pass "oversized FAQ payload is truncated to the byte budget" \
  || fail "oversized-FAQ rc=$RC payload=${OUT:0:200}"

# 7d. The truncation cut is UTF-8-safe: a multi-byte character straddling the
#     4000-byte boundary must not survive as a raw split byte. A raw `head -c`
#     cut there landed a lone/invalid UTF-8 byte in the JSON payload.
python3 -c "
prefix = 'Q: big?\nA: '
s = prefix + ('x' * 3988) + (chr(0xe9) * 20) + '\n---\n'
import sys
sys.stdout.write(s)
" > "$KING_FAQ_FIXTURE"
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID\"}")"
RC=$?
# A raw byte-boundary cut through this exact character lands a lone/invalid
# UTF-8 byte that json.dumps can only represent as an escaped lone surrogate
# (\udcXX, the D800-DFFF range) - grep for that escape rather than just
# json-parsing, since Python's own json.load tolerates a lone surrogate and
# would report the payload valid either way.
[[ $RC -eq 0 ]] && ! printf '%s' "$OUT" | grep -qE '\\ud[89a-f][0-9a-f]{2}' \
  && pass "UTF-8-straddling truncation carries no lone-surrogate escape" \
  || fail "UTF-8-straddling truncation rc=$RC leaked a lone surrogate: ${OUT:0:200}"

# 7e. Canon read-back: the precompact doc's FILLED judgment blocks ride back
#     after the compact; default placeholders (their signature text) do not.
registry_fixture "$CROWNED_ROW"
FNO_PLATFORM=claude
CANON_DOC="$TMP/canon-doc.md"
cat > "$CANON_DOC" <<'DOC'
# Canon doc: crown fno

Session id (authoritative): `sess-king`  |  refreshed by precompact-canon-doc.sh.

## Merge order and why (session)
<!-- fno:session -->
Ship the sibling's PR before the read-back wave.
SENTINEL_CANON_MERGE
<!-- /fno:session -->

## Open decisions awaiting the operator (session)
<!-- fno:session -->
_Open decisions awaiting the operator. Nothing external knows this. The session fills it at full context._
<!-- /fno:session -->
DOC
printf '%s\n' "$CANON_DOC" > "$KING_HANDOFF_PATH_FIXTURE"
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID\"}")"
RC=$?
[[ $RC -eq 0 ]] \
  && echo "$OUT" | jq -e '.hookSpecificOutput.additionalContext
      | contains("Your crown'"'"'s handoff") and contains("SENTINEL_CANON_MERGE") and contains("Full canon doc")' >/dev/null 2>&1 \
  && ! printf '%s' "$OUT" | grep -q "Nothing external knows this" \
  && pass "canon read-back: filled blocks injected, placeholders dropped" \
  || fail "canon read-back rc=$RC payload=${OUT:0:300}"
: > "$KING_HANDOFF_PATH_FIXTURE"

# 7f. No resolvable canon doc: no handoff section, base brief still present
#     (positive marker, not absence alone).
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID\"}")"
RC=$?
[[ $RC -eq 0 ]] && echo "$OUT" | grep -q "level 1 over fno" \
  && ! echo "$OUT" | grep -q "Your crown's handoff" \
  && pass "no canon doc: no handoff section, brief intact" \
  || fail "no-canon-doc rc=$RC payload=${OUT:0:300}"

# 7g. The fno:user block: the user's own words ride back verbatim under their
#     own heading, alongside the session-filled judgment blocks.
cat > "$CANON_DOC" <<'DOC'
# Canon doc: crown fno

Session id (authoritative): `sess-king`  |  refreshed by precompact-canon-doc.sh.

## Merge order and why (session)
<!-- fno:session -->
Ship the sibling's PR before the read-back wave.
SENTINEL_CANON_MERGE
<!-- /fno:session -->

## User notes (you write here; the machine only ever reads this)
<!-- fno:user -->
SENTINEL_USER_REPLY_TO_ME directly, not through the board.
<!-- /fno:user -->
DOC
printf '%s\n' "$CANON_DOC" > "$KING_HANDOFF_PATH_FIXTURE"
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID\"}")"
RC=$?
[[ $RC -eq 0 ]] \
  && echo "$OUT" | jq -e '.hookSpecificOutput.additionalContext
      | contains("User notes (from your canon doc)") and contains("SENTINEL_USER_REPLY_TO_ME directly, not through the board.")' >/dev/null 2>&1 \
  && pass "user block surfaced verbatim under its own heading" \
  || fail "user-block surfacing rc=$RC payload=${OUT:0:300}"

# 7h. Placeholder-only user block: no section, base brief intact. A drifted
#     placeholder copy would surface the seed line as if the user typed it.
python3 - "$CANON_DOC" <<'PY'
import re, sys
p = sys.argv[1]
s = open(p).read()
s = re.sub(r"(?s)<!-- fno:user -->\n.*?<!-- /fno:user -->",
           "<!-- fno:user -->\n_(write here; the machine reads this every refresh and never edits it)_\n<!-- /fno:user -->",
           s, count=1)
open(p, "w").write(s)
PY
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID\"}")"
RC=$?
[[ $RC -eq 0 ]] && echo "$OUT" | grep -q "level 1 over fno" \
  && ! echo "$OUT" | grep -q "User notes (from your canon doc)" \
  && pass "placeholder-only user block: silent, brief intact" \
  || fail "placeholder-only rc=$RC payload=${OUT:0:300}"

# 7i. An oversized user block is truncated to the byte budget, never
#     reinjected whole.
python3 - "$CANON_DOC" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
import re
s = re.sub(r"(?s)<!-- fno:user -->\n.*?<!-- /fno:user -->",
           "<!-- fno:user -->\n" + ("y" * 6000) + "\n<!-- /fno:user -->",
           s, count=1)
open(p, "w").write(s)
PY
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID\"}")"
RC=$?
[[ $RC -eq 0 ]] && echo "$OUT" | grep -q "truncated at" \
  && pass "oversized user block truncated to the byte budget" \
  || fail "oversized-user rc=$RC payload=${OUT:0:200}"
: > "$KING_HANDOFF_PATH_FIXTURE"

# 8. Summary id resolution: the NEWEST isCompactSummary entry (by line
#    position) has its node-id candidates resolved through `fno backlog get`
#    and injected as unresolved:/resolved: rows the king must act on.
registry_fixture "$CROWNED_ROW,{\"session_id\":\"worker-1\",\"harness_session_id\":\"full-worker-1\",\"name\":\"worker\",\"status\":\"live\"}"
export KING_BACKLOG_FIXTURE="$TMP/backlog-rows.json"
printf '[{"id":"cd-33334444","status":"in_progress","locked_by_harness":"claude","locked_by_harness_session":"worker-1"},{"id":"ee-77778888","error":"not found"}]\n' > "$KING_BACKLOG_FIXTURE"
SUMMARY_TX="$TMP/summary-transcript.jsonl"
cat > "$SUMMARY_TX" <<'EOF'
{"type":"user","message":"older context"}
{"type":"system","subtype":"compact_boundary","timestamp":"2026-09-17T14:58:05.496Z"}
{"type":"user","isCompactSummary":true,"summary":"older beat cites aa-99998888 which must not win"}
{"type":"assistant","message":"work continues"}
{"type":"system","subtype":"compact_boundary","timestamp":"2026-09-17T15:58:05.496Z"}
{"type":"user","isCompactSummary":true,"summary":"newest beat: ab-11112222 running, cd-33334444 claimed, ee-77778888 gone, uuid 12345678-abcd-1234-abcd-123456789012 is not an id"}
EOF
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID\",\"transcript_path\":\"$SUMMARY_TX\"}")"
RC=$?
[[ $RC -eq 0 ]] && printf '%s' "$OUT" | jq -e '.hookSpecificOutput.additionalContext
    | contains("The summary'"'"'s node ids, re-resolved")
      and contains("unresolved: ab-11112222")
      and contains("resolved: cd-33334444 status=in_progress locked_by=claude holder_live=yes")
      and contains("Act on these rows, not on the summary")' >/dev/null 2>&1 \
  && pass "summary ids: newest entry wins, unresolved + resolved rows injected" \
  || fail "summary-ids rc=$RC payload=${OUT:0:300}"

# 8a. The older summary entry's ids never win (line position, not timestamp).
printf '%s' "$OUT" | grep -q "aa-99998888" \
  && fail "older summary entry leaked into the payload" \
  || pass "older summary entry ignored (line position, not timestamp)"

# 8b. Whole uuids are masked before extraction: their inner hex groups
#     straddle the id grammar at word boundaries and are never node ids.
printf '%s' "$OUT" | grep -q "abcd-1234" \
  && fail "uuid fragment extracted as a candidate id" \
  || pass "uuid masked before id extraction"

# 8c. An error row from the batch read is unresolved, never resolved: the
#     verb reports the miss as data and the hook relays it as a correction.
printf '%s' "$OUT" | grep -q "unresolved: ee-77778888" \
  && pass "batch error row lands under unresolved" \
  || fail "error row not surfaced as unresolved: ${OUT:0:200}"
: > "$KING_BACKLOG_FIXTURE"

# 8d. Degrade: `fno backlog get` failing (empty stdout) adds no section and
#     never a fabricated all-unresolved list; the other sections still ride.
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID\",\"transcript_path\":\"$SUMMARY_TX\"}")"
RC=$?
[[ $RC -eq 0 ]] && printf '%s' "$OUT" | jq -e '.hookSpecificOutput.additionalContext
    | contains("level 1 over fno")
      and (contains("The summary'"'"'s node ids") | not)' >/dev/null 2>&1 \
  && pass "failed backlog read: no id section, brief intact, exit 0" \
  || fail "failed-read degrade rc=$RC payload=${OUT:0:300}"
unset KING_BACKLOG_FIXTURE

# 8e. A transcript with no isCompactSummary entry at all: no candidates, no
#     section, base brief intact.
printf '{"type":"user","message":"plain transcript, never compacted"}\n' > "$SUMMARY_TX"
OUT="$(run_king "{\"source\":\"compact\",\"session_id\":\"$SID\"}")"
RC=$?
[[ $RC -eq 0 ]] && printf '%s' "$OUT" | grep -q "level 1 over fno" \
  && ! printf '%s' "$OUT" | grep -q "The summary's node ids" \
  && pass "no summary entry: no id section, brief intact" \
  || fail "no-summary rc=$RC payload=${OUT:0:300}"

# 7. Byte budget: the brief is paid on every compaction of every king.
BRIEF_BYTES=$(wc -c < "$BRIEF" | tr -d '[:space:]')
[[ "$BRIEF_BYTES" -le "$BRIEF_MAX_BYTES" ]] \
  && pass "source brief within budget (${BRIEF_BYTES} <= ${BRIEF_MAX_BYTES} bytes)" \
  || fail "source brief over budget: ${BRIEF_BYTES} > ${BRIEF_MAX_BYTES} bytes"

echo ""
echo "king-postcompact-reinject: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
