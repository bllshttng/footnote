#!/usr/bin/env bash
# Test suite for the turn-block marker emission in hooks/inside-leg-report.sh
# (session-identity gate + continuation re-open).
#
# The hook writes OSC 133 markers to /dev/tty; FNO_TURN_MARKER_TTY redirects
# that sink to a file so we can assert what got emitted. XDG_RUNTIME_DIR is
# isolated so the first-writer pins never touch the real runtime dir, and
# FNO_AGENTS_BIN points at a no-op stub so the state report is inert.
#
# Tests:
#   T1  pane host emits C on working
#   T2  nested session (same pane/epoch, different id) emits NOTHING
#   T3  done re-opens C when a /target manifest is present (D + C)
#   T4  done without a manifest is D only (no re-open)
#   T5  non-pane (no FNO_PANE) emits nothing
#   T6  old server (no FNO_PANE_EPOCH) degrades to the presence gate (emits)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/inside-leg-report.sh"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); printf '[inside-leg] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[inside-leg] FAIL: %s\n' "$*" >&2; }

[[ -f "$HOOK" ]] || { fail "hook not found at $HOOK"; exit 1; }
command -v python3 >/dev/null 2>&1 || { printf '[inside-leg] SKIP: python3 not on PATH\n'; exit 77; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
STUB="$TMP/fno-agents"; printf '#!/bin/sh\nexit 0\n' >"$STUB"; chmod +x "$STUB"
RUNTIME="$TMP/run"   # isolated XDG_RUNTIME_DIR -> pin files land here

# run_hook <state> <session_id> [cwd] -> prints the captured marker bytes.
# Reads FNO_PANE / FNO_PANE_EPOCH / FNO_SESSION from the environment.
run_hook() {
  local state="$1" sid="$2" cwd="${3:-$TMP}"
  local sink; sink="$(mktemp "$TMP/sink.XXXXXX")"
  ( cd "$cwd" && printf '{"session_id":"%s"}' "$sid" | \
      FNO_TURN_MARKER_TTY="$sink" \
      XDG_RUNTIME_DIR="$RUNTIME" \
      FNO_AGENTS_BIN="$STUB" \
      FNO_PANE="${FNO_PANE:-}" \
      FNO_PANE_EPOCH="${FNO_PANE_EPOCH:-}" \
      FNO_SESSION="${FNO_SESSION:-}" \
      bash "$HOOK" "$state" ) >/dev/null 2>&1
  cat "$sink"
}
has() { grep -qa "$1"; }   # binary-safe substring test on stdin

# T1: pane host claims the pin and emits C.
export FNO_PANE=1 FNO_PANE_EPOCH=1000 FNO_SESSION=main
out="$(run_hook working host-1)"
has '133;C' <<<"$out" && pass "T1 host emits C on working" || fail "T1 host should emit C"

# T2: a nested session shares FNO_PANE/EPOCH/SESSION but lost the pin -> silent.
out="$(run_hook working nested-2)"
has '133' <<<"$out" && fail "T2 nested should be silent" || pass "T2 nested emits nothing"

# T3: done re-opens C when a /target manifest marks a looping session.
export FNO_PANE=2 FNO_PANE_EPOCH=2000 FNO_SESSION=main
PROJ="$TMP/proj"; mkdir -p "$PROJ/.fno"; printf 'x\n' >"$PROJ/.fno/target-state.md"
out="$(run_hook done host-3 "$PROJ")"
{ has '133;D' <<<"$out" && has '133;C' <<<"$out"; } \
  && pass "T3 done re-opens C under a /target manifest" || fail "T3 expected D + re-open C"

# T4: done with no manifest is a plain close (no re-open).
export FNO_PANE=3 FNO_PANE_EPOCH=3000 FNO_SESSION=main
out="$(run_hook done host-4 "$TMP")"
{ has '133;D' <<<"$out" && ! has '133;C' <<<"$out"; } \
  && pass "T4 done without manifest is D only" || fail "T4 expected D only"

# T5: no FNO_PANE -> not a pane, emit nothing.
unset FNO_PANE; export FNO_PANE_EPOCH=5000 FNO_SESSION=main
out="$(run_hook working host-5)"
has '133' <<<"$out" && fail "T5 non-pane should be silent" || pass "T5 non-pane emits nothing"

# T6: FNO_PANE set but no epoch -> degrade to the v1 presence gate (still emits).
export FNO_PANE=9; unset FNO_PANE_EPOCH; export FNO_SESSION=main
out="$(run_hook working host-6)"
has '133;C' <<<"$out" && pass "T6 no-epoch degrades to presence gate" || fail "T6 expected emit on degrade"

# T7: an EMPTY pin (a half-succeeded create, e.g. ENOSPC) must degrade to emit,
# not latch the host into permanent silence.
export FNO_PANE=7 FNO_PANE_EPOCH=7000 FNO_SESSION=main
mkdir -p "$RUNTIME/fno-turn-pins-${EUID:-0}"
: >"$RUNTIME/fno-turn-pins-${EUID:-0}/main-7-7000"   # pre-seed an empty pin
out="$(run_hook working host-7)"
has '133;C' <<<"$out" && pass "T7 empty pin degrades to emit" || fail "T7 expected emit on empty pin"

printf '[inside-leg] %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
