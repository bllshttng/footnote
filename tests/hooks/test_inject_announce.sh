#!/usr/bin/env bash
# test_inject_announce.sh
#
# Contract for the fleet-announcement boundary hook (x-8cfb): relays the
# reader's render once, stays silent when the reader is silent (the real
# reader's cursor makes the second boundary silent), fails open on a failing
# or missing binary, parses the session id from the hook stdin JSON, and maps
# a SessionStart source:compact to the compact boundary.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/inject-announce.sh"

[[ -f "$HOOK" ]] || { echo "FAIL: hook not found at $HOOK" >&2; exit 1; }

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t inject-announce-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# A fake `fno-agents` controls the reader output. FNO_ANNOUNCE_STUB_OUT is
# what it prints; FNO_ANNOUNCE_STUB_ONCE makes it print on the first call
# only (the real reader's cursor: unseen on the first boundary, seen on the
# next). $FNO_ANNOUNCE_ARGS_LOG captures the argv the hook built.
mkdir -p "$TMP/bin"
cat > "$TMP/bin/fno-agents" <<'STUB'
#!/usr/bin/env bash
[[ -n "${FNO_ANNOUNCE_ARGS_LOG:-}" ]] && printf '%s\n' "$*" >> "$FNO_ANNOUNCE_ARGS_LOG"
[[ -n "${FNO_ANNOUNCE_STUB_FAIL:-}" ]] && exit 7
if [[ -n "${FNO_ANNOUNCE_STUB_ONCE:-}" ]]; then
    if [[ -f "${FNO_ANNOUNCE_STUB_ONCE}" ]]; then
        exit 0
    fi
    touch "$FNO_ANNOUNCE_STUB_ONCE"
fi
[[ -n "${FNO_ANNOUNCE_STUB_OUT:-}" ]] && printf '%s\n' "$FNO_ANNOUNCE_STUB_OUT"
exit 0
STUB
chmod +x "$TMP/bin/fno-agents"

run_hook() { PATH="$TMP/bin:$PATH" bash "$HOOK" "${1:-prompt}"; }

# 1. First boundary: the render is relayed with exit 0, and the argv carries
#    the session id from the hook stdin JSON.
ARGS_LOG="$TMP/args.log"
RENDER='<fno_mail id="msg-abc123" kind="announce" from="op" subject="" expires="2026-09-14T00:00:00Z">one bus line</fno_mail>'
OUT="$(printf '%s' '{"session_id":"sess-abc123","prompt":"hi"}' | FNO_ANNOUNCE_ARGS_LOG="$ARGS_LOG" FNO_ANNOUNCE_STUB_OUT="$RENDER" run_hook prompt 2>/dev/null)"
RC=$?
[[ $RC -eq 0 ]] && pass "first boundary: exit 0" || fail "first boundary rc=$RC"
[[ "$OUT" == "$RENDER" ]] \
  && pass "first boundary: relays the render" || fail "first boundary: got: $OUT"
grep -q "announce read" "$ARGS_LOG" 2>/dev/null \
  && pass "argv: invokes the reader verb" || fail "argv: $(cat "$ARGS_LOG" 2>/dev/null)"
grep -q -- "--session-id sess-abc123" "$ARGS_LOG" 2>/dev/null \
  && pass "argv: session id from hook stdin" || fail "argv: no session id: $(cat "$ARGS_LOG" 2>/dev/null)"
grep -q -- "--boundary prompt" "$ARGS_LOG" 2>/dev/null \
  && pass "argv: prompt boundary passed" || fail "argv: no boundary: $(cat "$ARGS_LOG" 2>/dev/null)"

# 2. Second boundary: the reader (real: cursor; here: ONCE marker) prints on
#    the first call and is silent on the next; the hook injects nothing after.
ONCE="$TMP/once-marker"
ARGS_LOG2="$TMP/args2.log"
OUT="$(printf '%s' '{"session_id":"sess-abc123"}' | FNO_ANNOUNCE_ARGS_LOG="$ARGS_LOG2" FNO_ANNOUNCE_STUB_OUT="$RENDER" FNO_ANNOUNCE_STUB_ONCE="$ONCE" run_hook prompt 2>/dev/null)"
RC=$?
[[ $RC -eq 0 && "$OUT" == "$RENDER" ]] && pass "read-once: first boundary relays" \
  || fail "read-once first: rc=$RC out=$OUT"
OUT="$(printf '%s' '{"session_id":"sess-abc123"}' | FNO_ANNOUNCE_STUB_OUT="$RENDER" FNO_ANNOUNCE_STUB_ONCE="$ONCE" run_hook prompt 2>/dev/null)"
RC=$?
[[ $RC -eq 0 ]] && pass "second boundary: exit 0" || fail "second boundary rc=$RC"
[[ -z "$OUT" ]] && pass "second boundary: injects nothing (read once)" \
  || fail "second boundary: unexpected output: $OUT"

# 3. SessionStart source:compact remaps to the compact boundary.
ARGS_LOG3="$TMP/args3.log"
printf '%s' '{"session_id":"sess-abc123","source":"compact"}' \
  | FNO_ANNOUNCE_ARGS_LOG="$ARGS_LOG3" run_hook start >/dev/null 2>&1
grep -q -- "--boundary compact" "$ARGS_LOG3" 2>/dev/null \
  && pass "SessionStart source:compact remaps to --boundary compact" \
  || fail "compact remap failed: $(cat "$ARGS_LOG3" 2>/dev/null)"

# 4. A plain startup SessionStart keeps the start boundary.
ARGS_LOG4="$TMP/args4.log"
printf '%s' '{"session_id":"sess-abc123","source":"startup"}' \
  | FNO_ANNOUNCE_ARGS_LOG="$ARGS_LOG4" run_hook start >/dev/null 2>&1
grep -q -- "--boundary start" "$ARGS_LOG4" 2>/dev/null \
  && pass "plain SessionStart keeps --boundary start" \
  || fail "start boundary lost: $(cat "$ARGS_LOG4" 2>/dev/null)"

# 5. No session id in the hook JSON -> nothing, exit 0.
OUT="$(printf '%s' '{"prompt":"no session here"}' | run_hook prompt 2>/dev/null)"
RC=$?
[[ $RC -eq 0 && -z "$OUT" ]] && pass "no session id: silent, exit 0" \
  || fail "no session id: rc=$RC out=$OUT"

# 6. Reader failure -> silent, exit 0 (fail-open).
OUT="$(printf '%s' '{"session_id":"sess-1"}' | FNO_ANNOUNCE_STUB_FAIL=1 run_hook prompt 2>/dev/null)"
RC=$?
[[ $RC -eq 0 && -z "$OUT" ]] && pass "reader failure: silent, exit 0" \
  || fail "reader failure: rc=$RC out=$OUT"

# 7. fno-agents absent -> silent, exit 0.
OUT="$(printf '%s' '{"session_id":"sess-1"}' | PATH="/usr/bin:/bin" bash "$HOOK" prompt 2>/dev/null)"
RC=$?
[[ $RC -eq 0 && -z "$OUT" ]] && pass "binary absent: silent, exit 0" \
  || fail "binary absent: rc=$RC out=$OUT"

echo
echo "announce hook: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
