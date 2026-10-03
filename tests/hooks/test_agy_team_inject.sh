#!/usr/bin/env bash
# test_agy_crown_inject.sh
#
# Contract for the agy PreInvocation adapter: unseen announcements inject on
# EVERY invocation through `fno-agents announce read --harness agy` (the
# reader's own cursor dedups), the crown line still lands only on invocation
# 0 and after the announcements, both arrive as one injectSteps payload of
# ephemeralMessage steps, and every missing ingredient degrades to {} with
# exit 0.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/agy-team-inject.sh"

[[ -f "$HOOK" ]] || { echo "FAIL: hook not found at $HOOK" >&2; exit 1; }

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t agy-team-inject-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

RENDER='<fno_mail id="msg-abc123" kind="announce" from="op" subject="test-hold" expires="2026-10-02T00:00:00Z">All clear: the test hold is lifted.</fno_mail>'

# A fake `fno-agents`: prints one unseen announcement for `announce read`.
# $FNO_AGY_ARGS_LOG captures the argv the hook built.
mkdir -p "$TMP/bin"
cat > "$TMP/bin/fno-agents" <<'STUB'
#!/usr/bin/env bash
[[ -n "${FNO_AGY_ARGS_LOG:-}" ]] && printf '%s\n' "$*" >> "$FNO_AGY_ARGS_LOG"
[[ -n "${FNO_AGY_STUB_FAIL:-}" ]] && exit 7
[[ -n "${FNO_AGY_STUB_OUT:-}" ]] && printf '%s\n' "$FNO_AGY_STUB_OUT"
exit 0
STUB
chmod +x "$TMP/bin/fno-agents"

# A fake `fno`: answers `agents registry-json` with one crowned row when
# $FNO_AGY_CROWN_SID names a session, else an empty roster.
cat > "$TMP/bin/fno" <<'STUB'
#!/usr/bin/env bash
if [[ "${1:-} ${2:-}" == "agents registry-json" ]]; then
    if [[ -n "${FNO_AGY_CROWN_SID:-}" ]]; then
        printf '{"agents":[{"session_id":"%s","crown_level":"L1","crown_scope":"fno"}]}\n' "$FNO_AGY_CROWN_SID"
    else
        printf '{"agents":[]}\n'
    fi
fi
exit 0
STUB
chmod +x "$TMP/bin/fno"

# A jq+bash PATH with NEITHER fno binary, for the degrade tests.
mk_path() {
    mkdir -p "$1"
    for b in bash jq dirname cat printf head env; do
        s="$(command -v "$b" 2>/dev/null)" && ln -sf "$s" "$1/$b"
    done
}
NOBIN="$TMP/nobin"
mk_path "$NOBIN"

# The `fno` stub alone (no fno-agents anywhere on PATH): the crown read
# still works. The ambient PATH carries a real fno-agents, so absence must
# be constructed, not assumed.
NOAGENTS="$TMP/bin-noagents"
mk_path "$NOAGENTS"
ln -sf "$TMP/bin/fno" "$NOAGENTS/fno"

run_hook() { PATH="$TMP/bin:$PATH" bash "$HOOK"; }

nsteps() { printf '%s' "$1" | jq -r '(.injectSteps // []) | length' 2>/dev/null; }
step() { printf '%s' "$1" | jq -r ".injectSteps[$2].ephemeralMessage" 2>/dev/null; }

# 1. Invocation 3 + one unseen announcement: one step, exit 0, and the argv
#    carries the session id, harness and boundary.
ARGS_LOG="$TMP/args.log"
OUT="$(printf '%s' '{"conversationId":"conv-1","invocationNum":3}' | FNO_AGY_ARGS_LOG="$ARGS_LOG" FNO_AGY_STUB_OUT="$RENDER" run_hook 2>/dev/null)"
RC=$?
[[ $RC -eq 0 ]] && pass "invocation 3: exit 0" || fail "invocation 3: rc=$RC"
[[ "$(nsteps "$OUT")" == "1" ]] && pass "invocation 3: one step" \
  || fail "invocation 3: expected 1 step, got: $OUT"
[[ "$(step "$OUT" 0)" == "$RENDER" ]] && pass "invocation 3: announcement is the step" \
  || fail "invocation 3: step mismatch: $(step "$OUT" 0)"
grep -q -- "--session-id conv-1" "$ARGS_LOG" 2>/dev/null \
  && pass "argv: session id from conversationId" || fail "argv: $(cat "$ARGS_LOG" 2>/dev/null)"
grep -q "announce read" "$ARGS_LOG" 2>/dev/null \
  && pass "argv: invokes the reader verb" || fail "argv: wrong verb: $(cat "$ARGS_LOG" 2>/dev/null)"
grep -q -- "--harness agy" "$ARGS_LOG" 2>/dev/null \
  && pass "argv: harness agy" || fail "argv: no harness: $(cat "$ARGS_LOG" 2>/dev/null)"
grep -q -- "--boundary prompt" "$ARGS_LOG" 2>/dev/null \
  && pass "argv: prompt boundary" || fail "argv: no boundary: $(cat "$ARGS_LOG" 2>/dev/null)"

# 2. Invocation 0 + crown + announcement: two steps, announcements first.
OUT="$(printf '%s' '{"conversationId":"conv-2","invocationNum":0}' | FNO_AGY_CROWN_SID=conv-2 FNO_AGY_STUB_OUT="$RENDER" run_hook 2>/dev/null)"
RC=$?
[[ $RC -eq 0 ]] && pass "invocation 0: exit 0" || fail "invocation 0: rc=$RC"
[[ "$(nsteps "$OUT")" == "2" ]] && pass "invocation 0: two steps" \
  || fail "invocation 0: expected 2 steps, got: $OUT"
[[ "$(step "$OUT" 0)" == "$RENDER" ]] && pass "invocation 0: announcements first" \
  || fail "invocation 0: first step is not the announcement: $(step "$OUT" 0)"
[[ "$(step "$OUT" 1)" == "You are the king:"* ]] && pass "invocation 0: crown second" \
  || fail "invocation 0: second step is not the crown: $(step "$OUT" 1)"

# 3. Invocation 0 + crown, no fno-agents: the crown still lands alone.
OUT="$(printf '%s' '{"conversationId":"conv-3","invocationNum":0}' | PATH="$NOAGENTS:/usr/bin:/bin" FNO_AGY_CROWN_SID=conv-3 bash "$HOOK" 2>/dev/null)"
[[ "$(nsteps "$OUT")" == "1" && "$(step "$OUT" 0)" == "You are the king:"* ]] \
  && pass "no fno-agents: crown alone still lands" \
  || fail "no fno-agents: expected crown only, got: $OUT"

# 4. Invocation 3, no fno-agents: {} and exit 0.
OUT="$(printf '%s' '{"conversationId":"conv-4","invocationNum":3}' | PATH="$NOBIN:/usr/bin:/bin" bash "$HOOK" 2>/dev/null)"
RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "no binary: {} exit 0" \
  || fail "no binary: rc=$RC out=$OUT"

# 5. No conversationId in the hook JSON: {} and exit 0.
OUT="$(printf '%s' '{"invocationNum":3}' | run_hook 2>/dev/null)"
RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "no conversationId: {} exit 0" \
  || fail "no conversationId: rc=$RC out=$OUT"

# 6. Uncrowned session, silent reader: {} and exit 0.
OUT="$(printf '%s' '{"conversationId":"conv-5","invocationNum":0}' | run_hook 2>/dev/null)"
RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "uncrowned + no announcement: {} exit 0" \
  || fail "uncrowned: rc=$RC out=$OUT"

# 7. Reader failure on invocation 3: fail-open to {} and exit 0.
OUT="$(printf '%s' '{"conversationId":"conv-6","invocationNum":3}' | FNO_AGY_STUB_FAIL=1 run_hook 2>/dev/null)"
RC=$?
[[ $RC -eq 0 && "$OUT" == "{}" ]] && pass "reader failure: fail-open {} exit 0" \
  || fail "reader failure: rc=$RC out=$OUT"

# 8. Reader failure on invocation 0: the crown still lands alone.
OUT="$(printf '%s' '{"conversationId":"conv-7","invocationNum":0}' | FNO_AGY_CROWN_SID=conv-7 FNO_AGY_STUB_FAIL=1 run_hook 2>/dev/null)"
[[ "$(nsteps "$OUT")" == "1" && "$(step "$OUT" 0)" == "You are the king:"* ]] \
  && pass "reader failure + crown: crown still lands" \
  || fail "reader failure + crown: expected crown only, got: $OUT"

echo
echo "agy crown inject: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
