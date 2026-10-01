#!/usr/bin/env bash
# Test suite for the C14 picker signal in hooks/inside-leg-report.sh:
# a PreToolUse carrying tool_name AskUserQuestion reports state=blocked with
# the QUESTION TEXT (or the "asking the user" fallback when the payload
# carried none), and ExitPlanMode reports "plan approval requested" (the
# session is asking the operator, so mail waits); any other tool_name on
# PreToolUse keeps state=working, and the next PreToolUse or Stop clears the
# blocked state.
#
# Companion to tests/hooks/test_inside_leg_report_blocked.sh (the RPC side)
# and tests/hooks/test_inside_leg_report_markers.sh (the marker gate).
#
# Tests:
#   T1  PreToolUse + AskUserQuestion -> --state blocked --reason "asking the user" (fallback)
#   T2  PreToolUse + ExitPlanMode    -> --state blocked --reason "plan approval requested"
#   T3  PreToolUse + Bash            -> --state working (no reclassification)
#   T4  picker blocked, then a Bash PreToolUse -> working again (clears)
#   T5  a picker blocked report writes no OSC 133 byte (blocked stays silent)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/inside-leg-report.sh"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); printf '[inside-leg-picker] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[inside-leg-picker] FAIL: %s\n' "$*" >&2; }

[[ -f "$HOOK" ]] || { fail "hook not found at $HOOK"; exit 1; }
command -v python3 >/dev/null 2>&1 || { printf '[inside-leg-picker] SKIP: python3 not on PATH\n'; exit 77; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# The same argv-recording stub the blocked test uses.
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

# run_hook <state> <session_id> <calls_file> <runtime_dir> [tool_name] [hook_event_name]
run_hook() {
  local state="$1" sid="$2" calls="$3" rt="$4" tool="${5:-}" event="${6:-PreToolUse}"
  local sink; sink="$(mktemp "$TMP/sink.XXXXXX")"
  local payload
  payload=$(python3 -c 'import json,sys; d={"session_id": sys.argv[1]}
if sys.argv[2]: d["tool_name"] = sys.argv[2]
if sys.argv[3]: d["hook_event_name"] = sys.argv[3]
print(json.dumps(d))' "$sid" "$tool" "$event")
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

# T1
CALLS1="$TMP/calls1"; : >"$CALLS1"
RT1="$TMP/rt1"; mkdir -p "$RT1"
run_hook working sess-p1 "$CALLS1" "$RT1" "AskUserQuestion" >/dev/null
n="$(call_count "$CALLS1")"
if [[ "$n" == "1" ]]; then
  line="$(call_str "$CALLS1" 1)"
  if echo "$line" | grep -qF -- '--state blocked' \
      && echo "$line" | grep -qF -- '--reason asking the user'; then
    pass "T1 AskUserQuestion reclassified blocked/asking the user"
  else
    fail "T1 expected --state blocked + --reason, got: $line"
  fi
else
  fail "T1 expected one recorded call, got $n"
fi

# T2
CALLS2="$TMP/calls2"; : >"$CALLS2"
RT2="$TMP/rt2"; mkdir -p "$RT2"
run_hook working sess-p2 "$CALLS2" "$RT2" "ExitPlanMode" >/dev/null
if call_str "$CALLS2" 1 | grep -qF -- '--state blocked' \
    && call_str "$CALLS2" 1 | grep -qF -- '--reason plan approval requested'; then
  pass "T2 ExitPlanMode reclassified blocked/plan approval requested"
else
  fail "T2 expected blocked/plan approval requested, got: $(call_str "$CALLS2" 1)"
fi

# T3
CALLS3="$TMP/calls3"; : >"$CALLS3"
RT3="$TMP/rt3"; mkdir -p "$RT3"
run_hook working sess-p3 "$CALLS3" "$RT3" "Bash" >/dev/null
if call_str "$CALLS3" 1 | grep -qF -- '--state working' \
    && ! call_str "$CALLS3" 1 | grep -qF -- '--reason asking the user'; then
  pass "T3 a Bash tool call still reports working"
else
  fail "T3 expected plain working, got: $(call_str "$CALLS3" 1)"
fi

# T4
CALLS4="$TMP/calls4"; : >"$CALLS4"
RT4="$TMP/rt4"; mkdir -p "$RT4"
run_hook working sess-p4 "$CALLS4" "$RT4" "AskUserQuestion" >/dev/null
run_hook working sess-p4 "$CALLS4" "$RT4" "Bash" >/dev/null
n="$(call_count "$CALLS4")"
if [[ "$n" == "2" ]] \
    && call_str "$CALLS4" 1 | grep -qF -- '--state blocked' \
    && call_str "$CALLS4" 2 | grep -qF -- '--state working'; then
  pass "T4 the next Bash PreToolUse clears the picker blocked state"
else
  fail "T4 expected blocked then working, got $n: $(tr '\n' '|' <"$CALLS4")"
fi

# T5
CALLS5="$TMP/calls5"; : >"$CALLS5"
RT5="$TMP/rt5"; mkdir -p "$RT5"
export FNO_PANE=9 FNO_PANE_EPOCH=9000 FNO_SESSION=main
out="$(run_hook working host-9 "$CALLS5" "$RT5" "AskUserQuestion")"
unset FNO_PANE FNO_PANE_EPOCH FNO_SESSION
if echo "$out" | grep -qa '133'; then
  fail "T5 a picker blocked report should write no OSC 133 byte, got: $out"
else
  pass "T5 picker blocked writes no OSC 133 byte"
fi

printf '[inside-leg-picker] %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
