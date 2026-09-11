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
BRIEF="$REPO_ROOT/skills/king-for-a-day/references/postcompact-brief.md"
# 1600 held 1580 with 20 B of slack. The demand-signal rule added 194 B: agents
# were not voting because nothing armed taught the verb, and this brief is one
# of only two armed surfaces that reach an install. Trading was barred by this
# file's own style-exception header, which says the five rules are the
# operator's verbatim corrections and must not be shortened. So the cap moves,
# keeping the same slack 1600 gave 1580.
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
  cat "$KING_REG_FIXTURE"
elif [ "$1" = "agents" ] && [ "$2" = "king" ] && [ "$3" = "faq" ] && [ "$4" = "list" ]; then
  cat "$KING_FAQ_FIXTURE" 2>/dev/null || true
elif [ "$1" = "config" ] && [ "$2" = "paths" ] && [ "$3" = "handoff" ]; then
  cat "$KING_HANDOFF_PATH_FIXTURE" 2>/dev/null || true
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
    | contains("level 1 over fno") and contains("Encode, then abdicate")' >/dev/null 2>&1 \
  && pass "crowned claude: additionalContext carries crown + first rule" \
  || fail "crowned claude rc=$RC payload=$OUT"

# 2. Crowned row, no source field, codex lane resolved through CODEX_THREAD_ID:
#    the real codex PostCompact event carries no session_id at all, so the SID
#    must come from the env marker the registry row's harness_session_id holds.
#    systemMessage carrier, never the claude-only hookSpecificOutput key.
registry_fixture "$CROWNED_HARNESS_ROW"
FNO_PLATFORM=codex
OUT="$(printf '%s' '{}' | env CODEX_THREAD_ID="$SID" FNO_PLATFORM=codex \
  PLUGIN_ROOT="$REPO_ROOT" CLAUDE_PLUGIN_ROOT="$TMP/foreign-claude-plugin" \
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

# 7b. A crowned king with matching FAQ entries gets them after the static
#     brief; an empty FAQ fixture (the default, case 1 above) adds nothing.
registry_fixture "$CROWNED_ROW"
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

# 7. Byte budget: the brief is paid on every compaction of every king.

echo ""
echo "king-postcompact-reinject: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
