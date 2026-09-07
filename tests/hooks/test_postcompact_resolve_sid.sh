#!/usr/bin/env bash
# test_postcompact_resolve_sid.sh
#
# Unit tests for postcompact_resolve_sid in scripts/lib/postcompact-carrier.sh:
# which session id a post-compaction hook attaches to when the event payload
# carries none. Verifies: the explicit event sid wins; the transcript basename
# fallback holds; one codex id (either marker, or both agreeing) resolves; two
# DIFFERENT ids of one family resolve to NOTHING (the same degrade the
# identity resolvers enforce) instead of the table-first guess; and the
# no-harness generic branch applies the same rule across its marker set.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LIB="$REPO_ROOT/scripts/lib/postcompact-carrier.sh"

[[ -f "$LIB" ]] || { echo "FAIL: carrier lib not found at $LIB" >&2; exit 1; }
export REPO_ROOT

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

resolve_sid() { # $1 = event sid, $2 = transcript; marker env is inherited
  bash -c 'source "$REPO_ROOT/scripts/lib/postcompact-carrier.sh" && postcompact_resolve_sid "$1" "$2"' sid "$1" "$2"
}

# Every case runs in a subshell that clears the ambient marker env first, so
# the harness's own session env can never leak into a fixture.
clean_env() {
  unset CLAUDE_PLUGIN_ROOT CODEX_PLUGIN_ROOT FNO_PLATFORM CODEX_THREAD_ID \
    CODEX_SESSION_ID CLAUDE_CODE_SESSION_ID GEMINI_SESSION_ID
}

# 1. An explicit event sid wins over every marker.
OUT="$(clean_env; CODEX_THREAD_ID=env-id resolve_sid event-id '')"
[[ "$OUT" == "event-id" ]] && pass "event sid wins" || fail "event sid: got '$OUT'"

# 2. Transcript basename fallback (claude session id lives in the file name).
OUT="$(clean_env; resolve_sid '' '/tmp/0abc123-4def.jsonl')"
[[ "$OUT" == "0abc123-4def" ]] && pass "transcript basename fallback" || fail "transcript: got '$OUT'"

# 3. Codex lane, durable thread marker alone.
OUT="$(clean_env; FNO_PLATFORM=codex CODEX_THREAD_ID=thread-1 resolve_sid '' '')"
[[ "$OUT" == "thread-1" ]] && pass "codex thread alone" || fail "codex thread: got '$OUT'"

# 4. Codex lane, legacy session marker alone.
OUT="$(clean_env; FNO_PLATFORM=codex CODEX_SESSION_ID=legacy-1 resolve_sid '' '')"
[[ "$OUT" == "legacy-1" ]] && pass "codex legacy alone" || fail "codex legacy: got '$OUT'"

# 5. Codex lane, both markers carrying the same id.
OUT="$(clean_env; FNO_PLATFORM=codex CODEX_THREAD_ID=same-1 CODEX_SESSION_ID=same-1 resolve_sid '' '')"
[[ "$OUT" == "same-1" ]] && pass "codex agreeing dup" || fail "codex dup: got '$OUT'"

# 6. Codex lane, disagreeing ids: empty, never the table-first id.
OUT="$(clean_env; FNO_PLATFORM=codex CODEX_THREAD_ID=thread-2 CODEX_SESSION_ID=legacy-2 resolve_sid '' '')"
[[ -z "$OUT" ]] && pass "codex disagreement attaches nothing" || fail "codex conflict: got '$OUT'"

# 7. Claude lane keeps its own marker.
OUT="$(clean_env; FNO_PLATFORM=claude CLAUDE_CODE_SESSION_ID=claude-1 resolve_sid '' '')"
[[ "$OUT" == "claude-1" ]] && pass "claude marker" || fail "claude: got '$OUT'"

# 8. Generic branch (no FNO_PLATFORM, no CLAUDE_PLUGIN_ROOT), one marker.
OUT="$(clean_env; CODEX_THREAD_ID=only-1 resolve_sid '' '')"
[[ "$OUT" == "only-1" ]] && pass "generic single marker" || fail "generic single: got '$OUT'"

# 9. Generic branch, every marker agreeing.
OUT="$(clean_env; CODEX_THREAD_ID=agree-1 CLAUDE_CODE_SESSION_ID=agree-1 CODEX_SESSION_ID=agree-1 resolve_sid '' '')"
[[ "$OUT" == "agree-1" ]] && pass "generic agreeing markers" || fail "generic agree: got '$OUT'"

# 10. Generic branch, disagreeing markers: empty, never the table-first id.
OUT="$(clean_env; CODEX_THREAD_ID=codex-3 CLAUDE_CODE_SESSION_ID=claude-3 resolve_sid '' '')"
[[ -z "$OUT" ]] && pass "generic disagreement attaches nothing" || fail "generic conflict: got '$OUT'"

echo ""
echo "postcompact_resolve_sid: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
