#!/usr/bin/env bash
# test_normalize.sh - bash harness for the /mail skill write-verb normalizer (ab-7479fdb2).
#
# Verifies the deterministic recipient/body parse, smart-quote stripping, and
# the empty-recipient/empty-body refusal in skills/mail/scripts/normalize.sh.
# Self-contained: no pytest, no fno. Run:
#
#   bash skills/mail/tests/test_normalize.sh
#
# Exit 0 = all pass; non-zero = at least one failure (names printed).
#
# Covers US4: AC4-HP (normalize + run the genuine send) and AC4-ERR (refuse an
# empty body without running anything), plus reply and broadcast parsing.

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NORM="$HERE/../scripts/normalize.sh"

PASS=0
FAIL=0

# field <output> <key> -> prints the value for key=... (first match, single line)
field() { printf '%s\n' "$1" | sed -n "s/^$2=//p" | head -1; }

# body_all <output> -> everything after the LAST-emitted `body=` field, which may
# span multiple lines (the multiline-body contract). Strip the `body=` prefix
# only on the FIRST match so a body line that itself starts with `body=` is kept
# verbatim - mirrors how the model reads "everything after the first body=".
body_all() { printf '%s' "$1" | awk '/^body=/ && !flag {flag=1; sub(/^body=/,""); print; next} flag'; }

# run <verb> <input> -> echoes normalize.sh stdout
run() { bash "$NORM" --verb "$1" --input "$2"; }

# A timeout binary if one is available (CI/ubuntu has `timeout`; macOS may have
# `gtimeout` from coreutils). Used to prove the missing-flag guard cannot hang.
TIMEOUT_BIN=""
command -v timeout  >/dev/null 2>&1 && TIMEOUT_BIN=timeout
command -v gtimeout >/dev/null 2>&1 && TIMEOUT_BIN=gtimeout

check_eq() {
  local label="$1" got="$2" want="$3"
  if [[ "$got" == "$want" ]]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s\n  want: %q\n  got:  %q\n' "$label" "$want" "$got"
  fi
}

# --- AC4-HP: a normal quoted send --------------------------------------------
out="$(run send 'target "Hello world"')"
check_eq 'AC4-HP status'    "$(field "$out" status)"    'ok'
check_eq 'AC4-HP verb'      "$(field "$out" verb)"      'send'
check_eq 'AC4-HP recipient' "$(field "$out" recipient)" 'target'
check_eq 'AC4-HP body'      "$(field "$out" body)"      'Hello world'
check_eq 'AC4-HP to_project (empty)' "$(field "$out" to_project)" ''

# --- send, unquoted body -----------------------------------------------------
out="$(run send 'target Hello world')"
check_eq 'unquoted recipient' "$(field "$out" recipient)" 'target'
check_eq 'unquoted body'      "$(field "$out" body)"      'Hello world'

# --- send, smart (curly) quotes around the body ------------------------------
out="$(run send 'target “Hello world”')"
check_eq 'smartquote status' "$(field "$out" status)" 'ok'
check_eq 'smartquote body'   "$(field "$out" body)"   'Hello world'

# --- send, recipient wrapped in quotes ---------------------------------------
out="$(run send '"target" hi there')"
check_eq 'quoted recipient stripped' "$(field "$out" recipient)" 'target'
check_eq 'quoted recipient body'     "$(field "$out" body)"      'hi there'

# --- send, interior spaces preserved -----------------------------------------
out="$(run send 'target "keep   the   spaces"')"
check_eq 'interior spaces' "$(field "$out" body)" 'keep   the   spaces'

# --- AC4-ERR: empty body (explicit empty quotes) -----------------------------
out="$(run send 'target ""')"
check_eq 'AC4-ERR quoted-empty status' "$(field "$out" status)" 'error'

# --- AC4-ERR: empty body (curly empty quotes) --------------------------------
out="$(run send 'target “”')"
check_eq 'AC4-ERR curly-empty status' "$(field "$out" status)" 'error'

# --- AC4-ERR: recipient only, no body ----------------------------------------
out="$(run send 'target')"
check_eq 'AC4-ERR no-body status' "$(field "$out" status)" 'error'

# --- AC4-ERR: wholly empty input ---------------------------------------------
out="$(run send '')"
check_eq 'AC4-ERR empty-input status' "$(field "$out" status)" 'error'

# --- broadcast: dashless `project <X> <body>` --------------------------------
out="$(run send 'project regready "deploying now"')"
check_eq 'broadcast status'     "$(field "$out" status)"     'ok'
check_eq 'broadcast to_project' "$(field "$out" to_project)" 'regready'
check_eq 'broadcast recipient (empty)' "$(field "$out" recipient)" ''
check_eq 'broadcast body'       "$(field "$out" body)"       'deploying now'

# --- broadcast: dash back-compat `--to-project <X> <body>` -------------------
out="$(run send '--to-project regready hello')"
check_eq 'broadcast dash to_project' "$(field "$out" to_project)" 'regready'
check_eq 'broadcast dash body'       "$(field "$out" body)"       'hello'

# --- broadcast: missing body still refused -----------------------------------
out="$(run send 'project regready')"
check_eq 'broadcast no-body status' "$(field "$out" status)" 'error'

# --- reply HP ----------------------------------------------------------------
out="$(run reply 'msg-abc123 "thanks for the fix"')"
check_eq 'reply status' "$(field "$out" status)" 'ok'
check_eq 'reply verb'   "$(field "$out" verb)"   'reply'
check_eq 'reply msg_id' "$(field "$out" msg_id)" 'msg-abc123'
check_eq 'reply body'   "$(field "$out" body)"   'thanks for the fix'

# --- reply: empty body refused -----------------------------------------------
out="$(run reply 'msg-abc123')"
check_eq 'reply no-body status' "$(field "$out" status)" 'error'

# --- reply: empty msg-id refused ---------------------------------------------
out="$(run reply '')"
check_eq 'reply empty status' "$(field "$out" status)" 'error'

# --- multiline body preserved (quoted), not truncated at the first newline ---
out="$(run send "$(printf 'target "line1\nline2"')")"
check_eq 'multiline status'    "$(field "$out" status)" 'ok'
check_eq 'multiline recipient' "$(field "$out" recipient)" 'target'
check_eq 'multiline body'      "$(body_all "$out")" "$(printf 'line1\nline2')"

# --- multiline body preserved (unquoted) -------------------------------------
out="$(run send "$(printf 'target line1\nline2')")"
check_eq 'multiline unquoted body' "$(body_all "$out")" "$(printf 'line1\nline2')"

# --- multiline reply body preserved ------------------------------------------
out="$(run reply "$(printf 'msg-xyz "first\nsecond"')")"
check_eq 'multiline reply msg_id' "$(field "$out" msg_id)" 'msg-xyz'
check_eq 'multiline reply body'   "$(body_all "$out")" "$(printf 'first\nsecond')"

# --- whitespace-only body refused (a blank message is still empty) -----------
out="$(run send 'target "   "')"
check_eq 'whitespace-only body status' "$(field "$out" status)" 'error'

# --- whitespace-only body refused (curly quotes) -----------------------------
out="$(run send 'target “   ”')"
check_eq 'whitespace-only curly status' "$(field "$out" status)" 'error'

# --- body with real content keeps interior padding ---------------------------
out="$(run send 'target "  hi  "')"
check_eq 'padded body status' "$(field "$out" status)" 'ok'
check_eq 'padded body kept'   "$(body_all "$out")" '  hi  '

# --- multiline body whose later line starts with `body=` is kept verbatim -----
out="$(run send "$(printf 'target "line1\nbody=keptverbatim"')")"
check_eq 'body= interior line kept' "$(body_all "$out")" "$(printf 'line1\nbody=keptverbatim')"

# --- verb is case-insensitive (phone auto-capitalization) --------------------
out="$(run Send 'target hi')"
check_eq 'capitalized verb status' "$(field "$out" status)" 'ok'
check_eq 'capitalized verb body'   "$(field "$out" body)"   'hi'
out="$(run REPLY 'msg-x "yo"')"
check_eq 'caps reply status' "$(field "$out" status)" 'ok'
check_eq 'caps reply verb'   "$(field "$out" verb)"   'reply'

# --- missing flag value refused, never hangs (codex P2) ----------------------
# A bare `--verb` / `--input` with no following value must emit status=error and
# return immediately - not spin the arg loop forever.
if [[ -n "$TIMEOUT_BIN" ]]; then
  out="$("$TIMEOUT_BIN" 5 bash "$NORM" --verb 2>&1)"; rc=$?
  check_eq 'bare --verb did not hang' "$([[ $rc -eq 124 ]] && echo HANG || echo ok)" 'ok'
  check_eq 'bare --verb status'       "$(field "$out" status)" 'error'
  out="$("$TIMEOUT_BIN" 5 bash "$NORM" --verb send --input 2>&1)"; rc=$?
  check_eq 'bare --input did not hang' "$([[ $rc -eq 124 ]] && echo HANG || echo ok)" 'ok'
  check_eq 'bare --input status'       "$(field "$out" status)" 'error'
else
  # No timeout binary: still assert the status field (the fix makes it fast), but
  # skip the no-hang guard rather than risk hanging a local run on a regression.
  out="$(bash "$NORM" --verb 2>&1)"
  check_eq 'bare --verb status (no timeout bin)' "$(field "$out" status)" 'error'
fi

# --- unknown verb refused ----------------------------------------------------
out="$(run frobnicate 'target hi')"
check_eq 'unknown verb status' "$(field "$out" status)" 'error'

# -----------------------------------------------------------------------------
printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
