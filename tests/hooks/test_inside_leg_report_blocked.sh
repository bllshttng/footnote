#!/usr/bin/env bash
# Test suite for the `blocked` state producer in hooks/inside-leg-report.sh:
# Notification->blocked with a reason, the PreToolUse/report transition gate,
# and the OSC-marker exclusion for `blocked`. Companion to
# tests/hooks/test_inside_leg_report_markers.sh, which covers the turn-marker
# session-identity gate; this file covers the `fno-agents report` RPC side.
#
# Tests:
#   T1  Notification payload with a message -> one recorded call, --state
#       blocked and --reason <message>
#   T2  two consecutive `working` calls inside the refresh window -> one
#       recorded call (the transition gate)
#   T3  working, blocked, working -> three recorded calls, in that order
#   T4  blocked writes no OSC 133 byte; working writes 133;C (the positive
#       control, so a silent harness cannot pass by never running)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/inside-leg-report.sh"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); printf '[inside-leg-blocked] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[inside-leg-blocked] FAIL: %s\n' "$*" >&2; }

[[ -f "$HOOK" ]] || { fail "hook not found at $HOOK"; exit 1; }
command -v python3 >/dev/null 2>&1 || { printf '[inside-leg-blocked] SKIP: python3 not on PATH\n'; exit 77; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# A stub fno-agents that records every call's argv as one line, args joined by
# the unit separator (octal \037, NOT \x1f: dash's POSIX printf builtin -
# /bin/sh on the Linux CI runner - has no \xHH hex escape and would otherwise
# emit the four literal characters "\x1f" instead of the byte) so a reason
# string containing spaces stays unambiguous against the arg boundaries
# around it. $CALLS_FILE is exported by run_hook.
STUB="$TMP/fno-agents"
cat >"$STUB" <<'STUBEOF'
#!/bin/sh
out=""
for a in "$@"; do
  out="${out}${a}$(printf '\037')"
done
printf '%s\n' "$out" >> "$CALLS_FILE"
exit 0
STUBEOF
chmod +x "$STUB"

# run_hook <state> <session_id> <calls_file> <runtime_dir> [message] [hook_event_name]
# Pipes a Notification/PreToolUse-shaped payload through the hook and prints
# whatever landed on the (redirected) marker sink.
run_hook() {
  local state="$1" sid="$2" calls="$3" rt="$4" msg="${5:-}" event="${6:-}"
  local sink; sink="$(mktemp "$TMP/sink.XXXXXX")"
  local payload
  payload=$(python3 -c 'import json,sys; d={"session_id": sys.argv[1]}
if sys.argv[2]: d["message"] = sys.argv[2]
if sys.argv[3]: d["hook_event_name"] = sys.argv[3]
print(json.dumps(d))' "$sid" "$msg" "$event")
  ( printf '%s' "$payload" | \
      CALLS_FILE="$calls" \
      FNO_TURN_MARKER_TTY="$sink" \
      XDG_RUNTIME_DIR="$rt" \
      FNO_AGENTS_BIN="$STUB" \
      FNO_PANE="${FNO_PANE:-}" \
      FNO_PANE_EPOCH="${FNO_PANE_EPOCH:-}" \
      FNO_SESSION="${FNO_SESSION:-}" \
      bash "$HOOK" "$state" ) >/dev/null 2>&1
  cat "$sink" 2>/dev/null
}

call_count() { wc -l <"$1" 2>/dev/null | tr -d ' '; }
call_str() { sed -n "${2}p" "$1" | tr '\037' ' '; }

# T1: a Notification payload with a message yields one recorded call carrying
# --state blocked and --reason <message>.
CALLS1="$TMP/calls1"; : >"$CALLS1"
RT1="$TMP/rt1"; mkdir -p "$RT1"
run_hook blocked sess-1 "$CALLS1" "$RT1" "waiting on permission to run rm" >/dev/null
n="$(call_count "$CALLS1")"
if [[ "$n" == "1" ]]; then
  line="$(call_str "$CALLS1" 1)"
  if echo "$line" | grep -qF -- '--state blocked' \
      && echo "$line" | grep -qF -- '--reason waiting on permission to run rm'; then
    pass "T1 Notification->blocked carries the reason"
  else
    fail "T1 expected --state blocked + --reason in call, got: $line"
  fi
else
  fail "T1 expected exactly one recorded call, got $n"
fi

# T2: two consecutive `working` invocations inside the refresh window collapse
# to exactly one recorded call (the transition gate).
CALLS2="$TMP/calls2"; : >"$CALLS2"
RT2="$TMP/rt2"; mkdir -p "$RT2"
run_hook working sess-2 "$CALLS2" "$RT2" >/dev/null
run_hook working sess-2 "$CALLS2" "$RT2" >/dev/null
n="$(call_count "$CALLS2")"
[[ "$n" == "1" ]] && pass "T2 two consecutive working calls collapse to one" \
  || fail "T2 expected 1 recorded call, got $n"

# T3: working, blocked, working -> three recorded calls, in that order (each
# call changes the recorded state, so the gate never skips one).
CALLS3="$TMP/calls3"; : >"$CALLS3"
RT3="$TMP/rt3"; mkdir -p "$RT3"
run_hook working sess-3 "$CALLS3" "$RT3" >/dev/null
run_hook blocked sess-3 "$CALLS3" "$RT3" "idle wait" >/dev/null
run_hook working sess-3 "$CALLS3" "$RT3" >/dev/null
n="$(call_count "$CALLS3")"
if [[ "$n" == "3" ]] \
    && call_str "$CALLS3" 1 | grep -qF -- '--state working' \
    && call_str "$CALLS3" 2 | grep -qF -- '--state blocked' \
    && call_str "$CALLS3" 3 | grep -qF -- '--state working'; then
  pass "T3 working/blocked/working yields three calls in order"
else
  fail "T3 expected 3 calls in order working,blocked,working; got $n: $(tr '\n' '|' <"$CALLS3")"
fi

# T4: a blocked invocation writes no OSC 133 byte; a working one writes
# 133;C. Both driven from the SAME session so the first-writer pin (covered by
# test_inside_leg_report_markers.sh) does not itself explain a silent read.
CALLS4="$TMP/calls4"; : >"$CALLS4"
RT4="$TMP/rt4"; mkdir -p "$RT4"
export FNO_PANE=4 FNO_PANE_EPOCH=4000 FNO_SESSION=main
out_working="$(run_hook working host-4 "$CALLS4" "$RT4")"
out_blocked="$(run_hook blocked host-4 "$CALLS4" "$RT4" "idle wait")"
unset FNO_PANE FNO_PANE_EPOCH FNO_SESSION
if echo "$out_working" | grep -qa '133;C'; then
  if echo "$out_blocked" | grep -qa '133'; then
    fail "T4 blocked should write no OSC 133 byte, got: $out_blocked"
  else
    pass "T4 blocked silent, working emits 133;C (positive control)"
  fi
else
  fail "T4 positive control failed: working did not emit 133;C"
fi

# T5: a mid-turn `working` (PreToolUse, `blocked`'s inverse) must NOT open a
# turn-marker block, or a turn with N tool calls fragments into N sub-blocks;
# a turn-start `working` (UserPromptSubmit) must still open one (positive
# control). Same session/pane throughout so the first-writer pin cannot
# explain a silent read.
CALLS5="$TMP/calls5"; : >"$CALLS5"
RT5="$TMP/rt5"; mkdir -p "$RT5"
export FNO_PANE=5 FNO_PANE_EPOCH=5000 FNO_SESSION=main
out_ups="$(run_hook working host-5 "$CALLS5" "$RT5" "" "UserPromptSubmit")"
out_ptu="$(run_hook working host-5 "$CALLS5" "$RT5" "" "PreToolUse")"
unset FNO_PANE FNO_PANE_EPOCH FNO_SESSION
if echo "$out_ups" | grep -qa '133;C'; then
  if echo "$out_ptu" | grep -qa '133'; then
    fail "T5 PreToolUse-sourced working should write no OSC 133 byte, got: $out_ptu"
  else
    pass "T5 PreToolUse working silent, UserPromptSubmit working emits 133;C"
  fi
else
  fail "T5 positive control failed: UserPromptSubmit working did not emit 133;C"
fi

# T6: two `blocked` reports with DIFFERENT reasons, inside the refresh window,
# both land -- the transition gate must not dedup past a genuine reason
# change (a stale reason would otherwise sit in the daemon for up to the
# refresh ceiling).
CALLS6="$TMP/calls6"; : >"$CALLS6"
RT6="$TMP/rt6"; mkdir -p "$RT6"
run_hook blocked sess-6 "$CALLS6" "$RT6" "waiting on permission A" >/dev/null
run_hook blocked sess-6 "$CALLS6" "$RT6" "waiting on permission B" >/dev/null
n="$(call_count "$CALLS6")"
if [[ "$n" == "2" ]] \
    && call_str "$CALLS6" 1 | grep -qF -- '--reason waiting on permission A' \
    && call_str "$CALLS6" 2 | grep -qF -- '--reason waiting on permission B'; then
  pass "T6 blocked reports with different reasons both land"
else
  fail "T6 expected 2 calls with distinct reasons; got $n: $(tr '\n' '|' <"$CALLS6")"
fi

# T7: a FAILED report must not be persisted as sent -- two consecutive
# same-state calls against a stub that always fails/times out must BOTH
# attempt the RPC, or a transient daemon outage would leave the row stale for
# up to the refresh ceiling while the worker keeps running.
FAILSTUB="$TMP/fno-agents-fail"
cat >"$FAILSTUB" <<'FAILSTUBEOF'
#!/bin/sh
out=""
for a in "$@"; do
  out="${out}${a}$(printf '\037')"
done
printf '%s\n' "$out" >> "$CALLS_FILE"
exit 1
FAILSTUBEOF
chmod +x "$FAILSTUB"
CALLS7="$TMP/calls7"; : >"$CALLS7"
RT7="$TMP/rt7"; mkdir -p "$RT7"
sink7="$TMP/sink7"
( printf '{"session_id":"sess-7"}' | \
    CALLS_FILE="$CALLS7" FNO_TURN_MARKER_TTY="$sink7" XDG_RUNTIME_DIR="$RT7" \
    FNO_AGENTS_BIN="$FAILSTUB" bash "$HOOK" working ) >/dev/null 2>&1
( printf '{"session_id":"sess-7"}' | \
    CALLS_FILE="$CALLS7" FNO_TURN_MARKER_TTY="$sink7" XDG_RUNTIME_DIR="$RT7" \
    FNO_AGENTS_BIN="$FAILSTUB" bash "$HOOK" working ) >/dev/null 2>&1
n="$(call_count "$CALLS7")"
[[ "$n" == "2" ]] && pass "T7 a failed report is not persisted, retry is not suppressed" \
  || fail "T7 expected 2 attempted calls after a failing send, got $n"

printf '[inside-leg-blocked] %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
