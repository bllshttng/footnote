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
  env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL -u ANTHROPIC_AUTH_TOKEN -u FNO_HOME \
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

# 4b. Look-alike host: a foreign base whose host merely ENDS in "anthropic.com"
#     as a substring (notanthropic.com) must NOT trip the drift warning.
OUT="$(run_hook "glm-4.6" "https://notanthropic.com/api" "sk-real-apikey" 2>/dev/null)"
echo "$OUT" | grep -q "ROUTING DRIFT" && fail "false DRIFT on look-alike host notanthropic.com: $OUT" \
  || pass "notanthropic.com is not treated as an Anthropic host"

# 5. OAuth-scrub catch: foreign base but an Anthropic OAuth token.
OUT="$(run_hook "glm-4.6" "https://open.bigmodel.cn/api/anthropic" "sk-ant-oat-xxxx" 2>/dev/null)"
echo "$OUT" | grep -q "OAuth" && pass "oat token on routed lane -> OAuth warning" \
  || fail "expected OAuth warning, got: $OUT"

# 6. Sidecar recorded the intended identity.
[[ -f "$HOME/.fno/attest/${SID}.json" ]] && pass "sidecar written" || fail "sidecar missing"

# 7. Fail-open: garbage env still exits 0 and never blocks.
OUT="$(run_hook "!!!bad model!!!" "not-a-url" "" 2>/dev/null)"; RC=$?
[[ $RC -eq 0 ]] && pass "garbage env exits 0 (fail-open)" || fail "garbage env rc=$RC"

# 8. Parity: the drift predicate is duplicated in
#    skills/review/scripts/emit-attestation.sh, because a skill script may not
#    source outside its own directory. Two copies of one predicate is the shape
#    where a later edit fixes one and leaves the other, so drive BOTH over one
#    env matrix and fail when they disagree - a third copy added later inherits
#    this guarantee only by being added to the matrix, but a diverging edit to
#    either existing copy is caught here.
#
#    The hook reports drift by warning. The emitter reports it by blanking the
#    model it would otherwise stamp, so read its stderr summary: a non-empty
#    ANTHROPIC_MODEL that surfaces as `model=unobserved` was judged inert.
EMITTER="$REPO_ROOT/skills/review/scripts/emit-attestation.sh"
if [[ ! -f "$EMITTER" ]]; then
  fail "emitter not found at $EMITTER (parity matrix cannot run)"
else
  # Stub the event sink and CAPTURE ITS ARGV. Reading the emitter's stderr
  # receipt would test the wrong layer: the receipt and the stored payload are
  # what drifted apart in the first place (the summary printed "unobserved"
  # while the row carried ""), so the assertion has to read what was actually
  # handed to `event emit`.
  printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$@" > "%s/last-emit.txt"\nexit 0\n' \
    "$TMP" > "$TMP/fno-stub"
  chmod +x "$TMP/fno-stub"

  emitter_stored_model() { # args: MODEL BASE -> echoes the stored model value
    local _m="$1" _b="$2"
    rm -f "$TMP/last-emit.txt"
    (cd "$REPO_ROOT" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
      ANTHROPIC_MODEL="$_m" ANTHROPIC_BASE_URL="$_b" FNO="$TMP/fno-stub" \
      bash "$EMITTER" code-review pass) >/dev/null 2>&1 || { echo "<error>"; return; }
    [[ -f "$TMP/last-emit.txt" ]] || { echo "<no-emit>"; return; }
    # argv is: event emit -t <kind> -s <source> -d <json>; take the last line.
    tail -1 "$TMP/last-emit.txt" | jq -r '.model // "<missing>"'
  }

  emitter_drift() { # args: MODEL BASE -> echoes yes|no
    local _m="$1" _b="$2"
    [[ -z "$_m" ]] && { echo no; return; }   # no claim to refuse
    case "$(emitter_stored_model "$_m" "$_b")" in
      unobserved) echo yes ;;
      *) echo no ;;
    esac
  }

  hook_drift() { # args: MODEL BASE -> echoes yes|no
    case "$(run_hook "$1" "$2" "" 2>/dev/null)" in
      *"ROUTING DRIFT"*) echo yes ;;
      *) echo no ;;
    esac
  }

  # model|base|expected-drift
  MATRIX=(
    "||no"
    "claude-opus-4-8||no"
    "glm-4.6||yes"
    "glm-4.6|https://api.anthropic.com|yes"
    "glm-4.6|https://eu.anthropic.com|yes"
    "glm-4.6|https://open.bigmodel.cn/api/anthropic|no"
    "glm-4.6|https://notanthropic.com/api|no"
  )
  for row in "${MATRIX[@]}"; do
    IFS='|' read -r m b want <<<"$row"
    got_hook="$(hook_drift "$m" "$b")"
    got_emit="$(emitter_drift "$m" "$b")"
    label="model='${m:-<unset>}' base='${b:-<unset>}'"
    if [[ "$got_hook" == "$got_emit" && "$got_hook" == "$want" ]]; then
      pass "drift parity ($label) -> $want"
    else
      fail "drift parity ($label): want=$want hook=$got_hook emitter=$got_emit"
    fi
  done

  # 9. The three model states stay three distinct stored values. A refused claim
  #    and an absent one must never share a value: an empty field would have two
  #    explanations (nothing was set, versus something was set and rejected) and
  #    a reader could not tell them apart. Assert the positive marker.
  got="$(emitter_stored_model "" "")"
  [[ "$got" == "" ]] && pass "no claim in env stores the empty string" \
    || fail "no claim should store empty, stored '$got'"

  got="$(emitter_stored_model "claude-opus-4-8" "")"
  [[ "$got" == "claude-opus-4-8" ]] && pass "a coherent claim stores verbatim" \
    || fail "coherent claim stored '$got'"

  got="$(emitter_stored_model "glm-4.6" "")"
  [[ "$got" == "unobserved" ]] && pass "a refused claim stores the literal unobserved" \
    || fail "refused claim should store 'unobserved', stored '$got'"

  # And the receipt must not re-collapse what the payload separates.
  receipt="$(cd "$REPO_ROOT" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
    FNO="$TMP/fno-stub" bash "$EMITTER" code-review pass 2>&1 >/dev/null)"
  case "$receipt" in
    *"model=unobserved"*) fail "receipt calls an ABSENT claim 'unobserved': $receipt" ;;
    *"model=unset"*) pass "receipt distinguishes an absent claim from a refused one" ;;
    *) fail "receipt has no recognisable model field: $receipt" ;;
  esac
fi

echo ""
echo "attest-model: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
