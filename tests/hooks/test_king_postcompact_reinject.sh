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
BRIEF_MAX_BYTES=1600

[[ -f "$KING" ]] || { echo "FAIL: king hook not found at $KING" >&2; exit 1; }
export CLAUDE_PLUGIN_ROOT="$REPO_ROOT"

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t king-reinject-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# Stub `fno` answering `agents registry-json` from a per-case fixture file, so
# no real registry or daemon is involved. $KING_REG_FIXTURE selects the payload.
mkdir -p "$TMP/bin"
cat > "$TMP/bin/fno" <<'STUB'
#!/usr/bin/env bash
[ "$1" = "agents" ] && [ "$2" = "registry-json" ] || exit 1
cat "$KING_REG_FIXTURE"
STUB
chmod +x "$TMP/bin/fno"
export PATH="$TMP/bin:$PATH"
export KING_REG_FIXTURE="$TMP/registry.json"

SID="sess-king"
SID_OTHER="sess-someone-else"

# registry_fixture <row-json> - write a one-row registry around the given row.
registry_fixture() {
  printf '{"agents":[%s]}\n' "$1" > "$KING_REG_FIXTURE"
}

CROWNED_ROW='{"session_id":"'"$SID"'","harness_session_id":"full-'"$SID"'","name":"king","status":"live","crown_level":1,"crown_scope":"fno"}'
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
echo "$OUT" | jq -e '.hookSpecificOutput.additionalContext
    | contains("level 1 over fno") and contains("Encode, then abdicate")' >/dev/null 2>&1 \
  && pass "crowned claude: additionalContext carries crown + first rule" \
  || fail "crowned claude payload wrong: $OUT"
[[ $RC -eq 0 ]] && pass "crowned claude exits 0" || fail "crowned claude rc=$RC"

# 2. Crowned row, no source field, codex lane resolved through CODEX_THREAD_ID:
#    the real codex PostCompact event carries no session_id at all, so the SID
#    must come from the env marker the registry row's harness_session_id holds.
#    systemMessage carrier, never the claude-only hookSpecificOutput key.
FNO_PLATFORM=codex
OUT="$(printf '%s' '{}' | env CODEX_THREAD_ID="$SID" FNO_PLATFORM=codex bash "$KING" 2>/dev/null)"
RC=$?
echo "$OUT" | jq -e 'has("systemMessage") and (has("hookSpecificOutput") | not)' >/dev/null 2>&1 \
  && pass "crowned codex via CODEX_THREAD_ID: systemMessage carrier" \
  || fail "crowned codex payload wrong: $OUT"
[[ $RC -eq 0 ]] && pass "crowned codex exits 0" || fail "crowned codex rc=$RC"

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

# 7. Byte budget: the brief is paid on every compaction of every king.
BRIEF_BYTES="$(wc -c < "$BRIEF" 2>/dev/null | tr -d ' ')"
[[ -n "$BRIEF_BYTES" && "$BRIEF_BYTES" -le "$BRIEF_MAX_BYTES" ]] \
  && pass "brief is ${BRIEF_BYTES}B <= ${BRIEF_MAX_BYTES}B budget" \
  || fail "brief is ${BRIEF_BYTES:-missing}B, over the ${BRIEF_MAX_BYTES}B budget"

echo ""
echo "king-postcompact-reinject: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
