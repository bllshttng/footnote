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
# run_hook_env takes extra VAR=VAL assignments (tier vars, Bedrock/Vertex
# flags) after MODEL BASE TOKEN; run_hook is the bare three-arg form.
run_hook_env() {
  # Scrub ALL FIVE model vars plus the Bedrock/Vertex flags so the ambient
  # session (a routed shell, or a daemon-carried poisoned env) cannot leak
  # into the verdict: each case sets exactly what it asserts on and nothing
  # else.
  env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL -u ANTHROPIC_AUTH_TOKEN \
    -u ANTHROPIC_DEFAULT_OPUS_MODEL -u ANTHROPIC_DEFAULT_SONNET_MODEL \
    -u ANTHROPIC_DEFAULT_HAIKU_MODEL -u ANTHROPIC_DEFAULT_FABLE_MODEL \
    -u CLAUDE_CODE_USE_BEDROCK -u CLAUDE_CODE_USE_VERTEX -u FNO_HOME \
    ANTHROPIC_MODEL="$1" ANTHROPIC_BASE_URL="$2" ANTHROPIC_AUTH_TOKEN="$3" \
    HOME="$HOME" "${@:4}" \
    bash "$HOOK" <<<"$STDIN_JSON"
}
run_hook() { run_hook_env "$1" "$2" "$3"; }

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

# 3c. Tier-var-only poisoning: ANTHROPIC_MODEL unset, the haiku tier default
#     names a foreign model with no base URL. This is the shape that kills the
#     small tier (WebSearch / WebFetch summarization) while the interactive
#     selection looks fine. Assert the var NAME, not a generic warning marker:
#     a grep for "DRIFT" alone would pass on a warning about the wrong var.
OUT="$(run_hook_env "" "" "" ANTHROPIC_DEFAULT_HAIKU_MODEL="glm-4.5-air" 2>/dev/null)"
echo "$OUT" | grep -q "ANTHROPIC_DEFAULT_HAIKU_MODEL" \
  && pass "haiku-only poisoning names the tier var" \
  || fail "haiku-only poisoning went unseen: $OUT"

# 3d. Two poisoned vars report in ONE line naming both.
OUT="$(run_hook_env "glm-5.2[1m]" "" "" ANTHROPIC_DEFAULT_HAIKU_MODEL="glm-4.5-air" 2>/dev/null)"
LINES="$(printf '%s\n' "$OUT" | grep -c "MODEL ROUTING DRIFT")"
if [[ "$LINES" -eq 1 ]] \
  && printf '%s' "$OUT" | grep -q "ANTHROPIC_MODEL" \
  && printf '%s' "$OUT" | grep -q "ANTHROPIC_DEFAULT_HAIKU_MODEL"; then
  pass "two poisoned vars -> one line naming both"
else
  fail "expected one line naming both vars, got ($LINES lines): $OUT"
fi

# 3e. Bedrock: Anthropic models under us.anthropic.* ids with no base URL are
#     coherent (same for Vertex); the hook must print nothing. This was a
#     false positive in the single-var detector.
OUT="$(run_hook_env "us.anthropic.claude-sonnet-4-20250514-v1:0" "" "" \
  CLAUDE_CODE_USE_BEDROCK=1 2>/dev/null)"
[[ -z "$OUT" ]] && pass "Bedrock lane prints nothing" || fail "Bedrock warned: $OUT"
OUT="$(run_hook_env "us.anthropic.claude-sonnet-4-20250514-v1:0" "" "" \
  CLAUDE_CODE_USE_VERTEX=1 2>/dev/null)"
[[ -z "$OUT" ]] && pass "Vertex lane prints nothing" || fail "Vertex warned: $OUT"

# 3f. A mixed-case Anthropic id (or one with padding) is coherent: Python's
#     is_anthropic_model strips and lowercases before judging, and the hook
#     must agree rather than drift-warn on a var the spawn seams leave alone.
OUT="$(run_hook_env "Claude-Haiku-4-5" "" "" 2>/dev/null)"
[[ -z "$OUT" ]] && pass "mixed-case Anthropic id prints nothing" || fail "mixed-case id warned: $OUT"
OUT="$(run_hook_env "" "" "" "ANTHROPIC_DEFAULT_HAIKU_MODEL=opus " 2>/dev/null)"
[[ -z "$OUT" ]] && pass "padded bare alias prints nothing" || fail "padded alias warned: $OUT"

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

# 5b. Tier-default-only route: ANTHROPIC_MODEL unset, only
#     ANTHROPIC_DEFAULT_HAIKU_MODEL routed, real foreign base, Anthropic OAuth
#     token. The OAuth check must scan all five model vars, not just
#     ANTHROPIC_MODEL - a check gated on $MODEL alone would skip this case.
OUT="$(run_hook_env "" "https://open.bigmodel.cn/api/anthropic" "sk-ant-oat-xxxx" \
  ANTHROPIC_DEFAULT_HAIKU_MODEL=glm-4.5-air 2>/dev/null)"
echo "$OUT" | grep -q "OAuth" && pass "oat token on tier-default-only route -> OAuth warning" \
  || fail "expected OAuth warning on tier-default-only route, got: $OUT"

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
  #
  # Discriminate on the verb, like tests/hooks/test_code_review_attest.sh's
  # stub. The emitter shells `fno` for more than the event sink now (it clears
  # the review hold once a verdict exists for the head), and a stub that
  # captured EVERY call overwrote the payload under assertion with the last
  # unrelated one - `.model` then read empty and a refused claim was
  # indistinguishable from an absent one, which is the exact confusion the
  # assertions below exist to refuse.
  printf '#!/usr/bin/env bash\nif [[ "$1" == "doctor" && "$2" == "event" && "$3" == "emit" ]]; then printf "%%s\\n" "$@" > "%s/last-emit.txt"; fi\nexit 0\n' \
    "$TMP" > "$TMP/fno-stub"
  chmod +x "$TMP/fno-stub"

  # A branch-checked scratch repo to emit from. The emitter refuses a detached
  # HEAD (an empty branch field would mint a pre-branch-field event), and a CI
  # checkout IS detached - $REPO_ROOT is not a place this emitter can run from.
  # It also measures the diff under review and refuses a zero-line one, so the
  # fixture carries a base commit on origin/main plus a real feature commit.
  EMITREPO="$TMP/emitrepo"
  git init -q -b scratch/emit "$EMITREPO"
  git -C "$EMITREPO" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
  git -C "$EMITREPO" update-ref refs/remotes/origin/main "$(git -C "$EMITREPO" rev-parse HEAD)"
  echo body > "$EMITREPO/a.txt"
  git -C "$EMITREPO" add a.txt
  git -C "$EMITREPO" -c user.email=t@t -c user.name=t commit -qm feature

  emitter_stored_model() { # args: MODEL BASE -> echoes the stored model value
    local _m="$1" _b="$2"
    rm -f "$TMP/last-emit.txt"
    (cd "$EMITREPO" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
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
  receipt="$(cd "$EMITREPO" && env -u ANTHROPIC_MODEL -u ANTHROPIC_BASE_URL \
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
