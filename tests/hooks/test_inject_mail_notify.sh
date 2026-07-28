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

# A PATH carrying the stub plus only what the hook genuinely needs (jq, and bash
# + sleep for the stub), and NO timeout(1)/gtimeout(1) on any host. /usr/bin
# ships timeout on Linux, so "strip the PATH" alone would leave CI measuring the
# coreutils path. Omitting jq instead would be worse than useless: the hook
# exits 0 the moment jq is missing, so a hang case would pass in 5ms having
# tested nothing.
mkdir -p "$TMP/nocu"
for b in bash sleep jq dirname; do
  # fail, do not skip: a binary silently missing from this dir makes the hook
  # bail early, and the timing case below would then report a holding cap while
  # having run nothing at all.
  p="$(command -v "$b" 2>/dev/null)" || { fail "cannot build a coreutils-free PATH: $b not found"; continue; }
  ln -sf "$p" "$TMP/nocu/$b"
done
NOCU_PATH="$TMP/bin:$TMP/nocu"
run_hook_nocoreutils() { PATH="$NOCU_PATH" bash "$HOOK" </dev/null; }

# 1. Nonzero unread -> additionalContext carrying the notify-self line.
OUT="$(FNO_STUB_OUT='2 unread fno mail from alice, bob: run `fno mail unread`' run_hook 2>/dev/null)"; RC=$?
[[ $RC -eq 0 ]] && pass "unread: exit 0" || fail "unread rc=$RC"
echo "$OUT" | jq -e '.hookSpecificOutput.hookEventName == "UserPromptSubmit"' >/dev/null 2>&1 \
  && pass "unread: emits UserPromptSubmit hookSpecificOutput" || fail "unread: bad envelope: $OUT"
# grep, not grep -q: under `set -o pipefail` grep -q exits on first match and
# SIGPIPEs upstream jq (exit 141), which would flake the pipeline. Redirecting
# to /dev/null consumes the whole stream.
echo "$OUT" | jq -r '.hookSpecificOutput.additionalContext' 2>/dev/null | grep "2 unread fno mail" >/dev/null \
  && pass "unread: additionalContext carries the nudge" || fail "unread: nudge missing: $OUT"
echo "$OUT" | jq -r '.hookSpecificOutput.additionalContext' 2>/dev/null | grep "system-reminder" >/dev/null \
  && pass "unread: wrapped in a system-reminder" || fail "unread: no wrapper: $OUT"

# 2. Empty notify-self -> nothing injected (no blank <system-reminder>).
OUT="$(FNO_STUB_OUT='' run_hook 2>/dev/null)"; RC=$?
[[ $RC -eq 0 ]] && pass "empty: exit 0" || fail "empty rc=$RC"
[[ -z "$OUT" ]] && pass "empty: injects nothing" || fail "empty: unexpected output: $OUT"

# 3. Missing fno -> silent no-op, turn proceeds (exit 0, no output).
OUT="$(PATH="/usr/bin:/bin" bash "$HOOK" </dev/null 2>/dev/null)"; RC=$?
[[ $RC -eq 0 ]] && pass "no-fno: exit 0" || fail "no-fno rc=$RC"
[[ -z "$OUT" ]] && pass "no-fno: injects nothing" || fail "no-fno: unexpected output: $OUT"

# 4. Hung binary -> the 2s cap bounds it; the hook still exits 0 quickly. Run on
#    a PATH with no timeout(1) at all: this is a UserPromptSubmit hook, so an
#    uncapped hang costs 10s on EVERY prompt, and stock macOS is that host.
if ( PATH="$NOCU_PATH"; command -v timeout || command -v gtimeout ) >/dev/null 2>&1; then
  fail "timeout: the coreutils-free PATH still resolves a timeout binary; the bound below asserts nothing"
elif [[ ! -x "$TMP/nocu/jq" ]]; then
  fail "timeout: jq missing from the coreutils-free PATH; the hook would exit 0 before reaching the cap"
else
  # Positive control first: the hook must still WORK on this PATH. Otherwise any
  # missing dependency makes it exit 0 in milliseconds and the timing assertion
  # below reports a holding cap while nothing ran.
  OUT="$(FNO_STUB_OUT='1 unread fno mail from alice: run `fno mail unread`' run_hook_nocoreutils 2>/dev/null)"
  echo "$OUT" | jq -e '.hookSpecificOutput.hookEventName == "UserPromptSubmit"' >/dev/null 2>&1 \
    && pass "timeout control: hook works on the coreutils-free PATH" \
    || fail "timeout control: hook produced no envelope on the coreutils-free PATH, so the bound below proves nothing: $OUT"

  START=$(date +%s)
  OUT="$(FNO_STUB_SLEEP=10 FNO_STUB_OUT='late' run_hook_nocoreutils 2>/dev/null)"; RC=$?
  END=$(date +%s)
  [[ $RC -eq 0 ]] && pass "timeout: exit 0" || fail "timeout rc=$RC"
  # Ceiling AND floor. `< 8` alone is satisfied by a hook that never reached the
  # stub, which is how a cap assertion reports success having tested nothing.
  (( END - START < 8 )) && pass "timeout: bounded without coreutils (<8s, not 10s)" || fail "timeout: not bounded ($((END - START))s)"
  (( END - START >= 1 )) && pass "timeout: actually waited for the cap (not an early exit)" || fail "timeout: returned in $((END - START))s, too fast to have run the 10s stub - it exited early and this case tested nothing"
  [[ -z "$OUT" ]] && pass "timeout: injects nothing when capped" || fail "timeout: unexpected output: $OUT"
fi

echo ""
echo "inject-mail-notify: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
