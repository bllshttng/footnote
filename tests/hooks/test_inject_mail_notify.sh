#!/usr/bin/env bash
# test_inject_mail_notify.sh
#
# Unit tests for hooks/inject-mail-notify.sh (x-39a4 task 1.4, the push-first
# turn-boundary mail nudge). Verifies: nonzero notify-self output is wrapped as
# UserPromptSubmit additionalContext; empty output injects nothing; a missing
# fno is a silent no-op; a hung binary is bounded by the timeout; the hook
# always exits 0.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/inject-mail-notify.sh"

[[ -f "$HOOK" ]] || { echo "FAIL: hook not found at $HOOK" >&2; exit 1; }

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t inject-mail-notify-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# A fake `fno` on PATH stands in for the real binary. $FNO_STUB_OUT is what
# `fno mail notify-self` prints; $FNO_STUB_SLEEP optionally hangs it.
mkdir -p "$TMP/bin"
cat > "$TMP/bin/fno" <<'STUB'
#!/usr/bin/env bash
[[ -n "${FNO_STUB_SLEEP:-}" ]] && sleep "$FNO_STUB_SLEEP"
[[ -n "${FNO_STUB_OUT:-}" ]] && printf '%s\n' "$FNO_STUB_OUT"
exit 0
STUB
chmod +x "$TMP/bin/fno"

run_hook() { PATH="$TMP/bin:$PATH" bash "$HOOK" </dev/null; }

# 1. Nonzero unread -> additionalContext carrying the notify-self line.
OUT="$(FNO_STUB_OUT='2 unread fno mail from alice, bob: run `fno mail unread`' run_hook 2>/dev/null)"; RC=$?
[[ $RC -eq 0 ]] && pass "unread: exit 0" || fail "unread rc=$RC"
echo "$OUT" | jq -e '.hookSpecificOutput.hookEventName == "UserPromptSubmit"' >/dev/null 2>&1 \
  && pass "unread: emits UserPromptSubmit hookSpecificOutput" || fail "unread: bad envelope: $OUT"
echo "$OUT" | jq -r '.hookSpecificOutput.additionalContext' 2>/dev/null | grep -q "2 unread fno mail" \
  && pass "unread: additionalContext carries the nudge" || fail "unread: nudge missing: $OUT"
echo "$OUT" | jq -r '.hookSpecificOutput.additionalContext' 2>/dev/null | grep -q "system-reminder" \
  && pass "unread: wrapped in a system-reminder" || fail "unread: no wrapper: $OUT"

# 2. Empty notify-self -> nothing injected (no blank <system-reminder>).
OUT="$(FNO_STUB_OUT='' run_hook 2>/dev/null)"; RC=$?
[[ $RC -eq 0 ]] && pass "empty: exit 0" || fail "empty rc=$RC"
[[ -z "$OUT" ]] && pass "empty: injects nothing" || fail "empty: unexpected output: $OUT"

# 3. Missing fno -> silent no-op, turn proceeds (exit 0, no output).
OUT="$(PATH="/usr/bin:/bin" bash "$HOOK" </dev/null 2>/dev/null)"; RC=$?
[[ $RC -eq 0 ]] && pass "no-fno: exit 0" || fail "no-fno rc=$RC"
[[ -z "$OUT" ]] && pass "no-fno: injects nothing" || fail "no-fno: unexpected output: $OUT"

# 4. Hung binary -> the 2s timeout bounds it; the hook still exits 0 quickly.
#    (Only assert when a timeout mechanism exists; otherwise skip the bound.)
if command -v timeout >/dev/null 2>&1 || command -v gtimeout >/dev/null 2>&1; then
  START=$(date +%s)
  OUT="$(FNO_STUB_SLEEP=10 FNO_STUB_OUT='late' run_hook 2>/dev/null)"; RC=$?
  END=$(date +%s)
  [[ $RC -eq 0 ]] && pass "timeout: exit 0" || fail "timeout rc=$RC"
  (( END - START < 8 )) && pass "timeout: bounded (<8s, not 10s)" || fail "timeout: not bounded ($((END - START))s)"
else
  pass "timeout: skipped (no timeout/gtimeout on PATH)"
fi

echo ""
echo "inject-mail-notify: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
