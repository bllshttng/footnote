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
#   T7  empty pin (half-failed create) degrades to emit, not permanent silence
#   T8  symlinked rendezvous dir degrades to emit, no hijack of the link target
#   T9  path-traversal FNO_SESSION is sanitized, pin stays contained
#   T10 malformed payload still emits (markers independent of the parse)
#   T11 non-numeric FNO_PANE degrades to emit, no path escape

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
RUNTIME="$TMP/run"; mkdir -p "$RUNTIME"   # isolated XDG_RUNTIME_DIR (parent must exist for the hook's no-`-p` mkdir)

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

# T8: a symlinked rendezvous dir must be refused (mkdir fails on it, the
# non-symlink check rejects it) -> degrade to emit, and the link target is never
# created/written (no hijack). Isolated runtime so it can't disturb T1-T7's pins.
RT2="$TMP/run2"; mkdir -p "$RT2"
ln -s "$TMP/hijack-target" "$RT2/fno-turn-pins-${EUID:-0}"
sink8="$TMP/sink8"
( cd "$TMP" && printf '{"session_id":"host-8"}' | \
    FNO_TURN_MARKER_TTY="$sink8" XDG_RUNTIME_DIR="$RT2" FNO_AGENTS_BIN="$STUB" \
    FNO_PANE=8 FNO_PANE_EPOCH=8000 FNO_SESSION=main bash "$HOOK" working ) >/dev/null 2>&1
{ has '133;C' <"$sink8" && [[ ! -e "$TMP/hijack-target" ]]; } \
  && pass "T8 symlinked pin dir degrades to emit, no hijack" || fail "T8 expected degrade-emit + untouched link target"

# T9: a path-traversal FNO_SESSION is sanitized so the pin never escapes the
# rendezvous dir. Without sanitization the pin would land at $TMP/escape-9-9000.
RT3="$TMP/run3"; mkdir -p "$RT3"
sink9="$TMP/sink9"
( cd "$TMP" && printf '{"session_id":"host-9"}' | \
    FNO_TURN_MARKER_TTY="$sink9" XDG_RUNTIME_DIR="$RT3" FNO_AGENTS_BIN="$STUB" \
    FNO_PANE=9 FNO_PANE_EPOCH=9000 FNO_SESSION="../../escape" bash "$HOOK" working ) >/dev/null 2>&1
{ has '133;C' <"$sink9" && [[ -z "$(find "$TMP" -maxdepth 1 -name '*escape*' 2>/dev/null)" ]]; } \
  && pass "T9 traversal FNO_SESSION sanitized, pin stays contained" || fail "T9 expected contained pin (no escape at TMP root)"

# T10: a malformed payload (no session_id) must NOT suppress markers -- the pane
# host still emits via the presence-gate degrade (regression guard for the parse
# reorder); only the state report is skipped.
RT4="$TMP/run4"; mkdir -p "$RT4"; sink10="$TMP/sink10"
( cd "$TMP" && printf '{}' | \
    FNO_TURN_MARKER_TTY="$sink10" XDG_RUNTIME_DIR="$RT4" FNO_AGENTS_BIN="$STUB" \
    FNO_PANE=10 FNO_PANE_EPOCH=10000 FNO_SESSION=main bash "$HOOK" working ) >/dev/null 2>&1
has '133;C' <"$sink10" && pass "T10 malformed payload still emits (markers independent of parse)" || fail "T10 expected emit on malformed payload"

# T11: a non-numeric (traversal) FNO_PANE is rejected -> degrade to emit, no file
# escapes the rendezvous dir.
RT5="$TMP/run5"; mkdir -p "$RT5"; sink11="$TMP/sink11"
( cd "$TMP" && printf '{"session_id":"host-11"}' | \
    FNO_TURN_MARKER_TTY="$sink11" XDG_RUNTIME_DIR="$RT5" FNO_AGENTS_BIN="$STUB" \
    FNO_PANE="../../pwn" FNO_PANE_EPOCH=11000 FNO_SESSION=main bash "$HOOK" working ) >/dev/null 2>&1
{ has '133;C' <"$sink11" && [[ -z "$(find "$TMP" -maxdepth 1 -name '*pwn*' 2>/dev/null)" ]]; } \
  && pass "T11 non-numeric FNO_PANE degrades to emit, no escape" || fail "T11 expected contained degrade-emit"

printf '[inside-leg] %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
