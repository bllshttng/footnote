#!/usr/bin/env bash
# test_attest_model.sh
#
# Unit tests for hooks/attest-model.sh (guard (a) Layer 1: model/provider env
# coherence). Verifies the x-db50 catch (foreign model + Anthropic base ->
# warning), coherent env silence, fail-open on a broken env, and the sidecar
# write. The hook is advisory: it must ALWAYS exit 0.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/attest-model.sh"

[[ -f "$HOOK" ]] || { echo "FAIL: hook not found at $HOOK" >&2; exit 1; }

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t attest-model-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# Isolate the sidecar under a fake HOME so a real ~/.claude is never touched.
export HOME="$TMP"
mkdir -p "$HOME/.claude"
SID="test-session-abc"
STDIN_JSON="$(printf '{"session_id":"%s"}' "$SID")"

# Run the hook with a scrubbed routing env, capturing stdout + exit code.
run_hook() {
  # args: MODEL BASE TOKEN
  env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL -u ANTHROPIC_AUTH_TOKEN \
    ANTHROPIC_MODEL="$1" ANTHROPIC_BASE_URL="$2" ANTHROPIC_AUTH_TOKEN="$3" \
    HOME="$HOME" \
    bash "$HOOK" <<<"$STDIN_JSON"
}

# 1. Coherent: un-routed Anthropic session -> no warning, exit 0.
OUT="$(run_hook "" "" "" 2>/dev/null)"; RC=$?
[[ $RC -eq 0 ]] && pass "unrouted env exits 0" || fail "unrouted env rc=$RC"
[[ -z "$OUT" ]] && pass "unrouted env emits no warning" || fail "unrouted warned: $OUT"

# 2. Coherent: Anthropic model, any base -> no warning.
OUT="$(run_hook "claude-opus-4-8" "" "" 2>/dev/null)"
[[ -z "$OUT" ]] && pass "anthropic model is coherent (silent)" || fail "anthropic warned: $OUT"

# 3. x-db50 catch: foreign model + Anthropic (empty) base -> drift warning.
OUT="$(run_hook "glm-4.6" "" "" 2>/dev/null)"; RC=$?
[[ $RC -eq 0 ]] && pass "mismatch exits 0 (advisory)" || fail "mismatch rc=$RC"
echo "$OUT" | grep -q "ROUTING DRIFT" && pass "foreign model + empty base -> DRIFT warning" \
  || fail "expected DRIFT warning, got: $OUT"

# 3b. Foreign model + explicit anthropic.com base -> drift warning.
OUT="$(run_hook "glm-4.6" "https://api.anthropic.com" "" 2>/dev/null)"
echo "$OUT" | grep -q "ROUTING DRIFT" && pass "foreign model + anthropic host -> DRIFT warning" \
  || fail "expected DRIFT warning for anthropic host, got: $OUT"

# 4. Properly routed: foreign model + foreign base -> no drift warning.
OUT="$(run_hook "glm-4.6" "https://open.bigmodel.cn/api/anthropic" "sk-real-apikey" 2>/dev/null)"
echo "$OUT" | grep -q "ROUTING DRIFT" && fail "false DRIFT on a real routed lane: $OUT" \
  || pass "foreign model + foreign base is coherent (no drift)"

# 5. OAuth-scrub catch: foreign base but an Anthropic OAuth token.
OUT="$(run_hook "glm-4.6" "https://open.bigmodel.cn/api/anthropic" "sk-ant-oat-xxxx" 2>/dev/null)"
echo "$OUT" | grep -q "OAuth" && pass "oat token on routed lane -> OAuth warning" \
  || fail "expected OAuth warning, got: $OUT"

# 6. Sidecar recorded the intended identity.
[[ -f "$HOME/.claude/.fno-attest-${SID}.json" ]] && pass "sidecar written" || fail "sidecar missing"

# 7. Fail-open: garbage env still exits 0 and never blocks.
OUT="$(run_hook "!!!bad model!!!" "not-a-url" "" 2>/dev/null)"; RC=$?
[[ $RC -eq 0 ]] && pass "garbage env exits 0 (fail-open)" || fail "garbage env rc=$RC"

echo ""
echo "attest-model: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
