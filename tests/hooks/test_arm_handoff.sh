#!/usr/bin/env bash
# test_arm_handoff.sh
#
# Unit tests for guard (c): hooks/arm-handoff-precompact.sh (PreCompact intent
# recorder) and the re-surface path in hooks/target-postcompact-reinject.sh.
# Verifies: arm on pressure + outstanding work; no arm on <promise>; no arm
# below threshold; no arm when the done sentinel exists; decline on unreadable
# transcript; the manifest is never mutated; PostCompact re-surfaces the marker.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ARM="$REPO_ROOT/hooks/arm-handoff-precompact.sh"
REINJECT="$REPO_ROOT/hooks/target-postcompact-reinject.sh"

[[ -f "$ARM" ]] || { echo "FAIL: arm hook not found at $ARM" >&2; exit 1; }
export CLAUDE_PLUGIN_ROOT="$REPO_ROOT"

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t arm-handoff-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT
SID="sess-xyz"

# Build a workspace: manifest owned by THIS (live) pid + a transcript at a given
# used_pct. $1=used_tokens (window is 200000, so used_pct = tokens/2000).
setup_ws() {
  local dir="$1" used_tokens="$2" last_text="$3"
  rm -rf "$dir"; mkdir -p "$dir/.fno"
  cat > "$dir/.fno/target-state.md" <<EOF
---
session_id: $SID
owner_pid: $$
graph_node_id: x-test
input: x-test
plan_path: /tmp/plan
---
EOF
  # Minimal transcript: one assistant line carrying usage + text.
  printf '{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":%s,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"content":[{"type":"text","text":"%s"}]}}\n' \
    "$used_tokens" "$last_text" > "$dir/transcript.jsonl"
}

run_arm() { # dir  -> runs the hook from within dir with transcript on stdin
  local dir="$1"
  ( cd "$dir" && printf '{"transcript_path":"%s/transcript.jsonl"}' "$dir" | bash "$ARM" )
}

# 1. Pressure (80%) + outstanding work -> arm marker written.
setup_ws "$TMP/a" 160000 "still working on it"
MANIFEST_BEFORE="$(cat "$TMP/a/.fno/target-state.md")"
run_arm "$TMP/a"
[[ -f "$TMP/a/.fno/.handoff-armed-$SID" ]] && pass "arms on pressure + outstanding work" \
  || fail "expected arming marker, none written"
grep -q '"node_id":"x-test"' "$TMP/a/.fno/.handoff-armed-$SID" && pass "marker carries node_id" \
  || fail "marker missing node_id"
[[ "$(cat "$TMP/a/.fno/target-state.md")" == "$MANIFEST_BEFORE" ]] \
  && pass "manifest NOT mutated (invariant)" || fail "manifest was mutated"

# 2. <promise> present -> no arm (session finishing).
setup_ws "$TMP/b" 160000 "<promise>MISSION COMPLETE: done</promise>"
run_arm "$TMP/b"
[[ ! -f "$TMP/b/.fno/.handoff-armed-$SID" ]] && pass "no arm when <promise> present" \
  || fail "armed despite <promise>"

# 3. Below threshold (10%) -> no arm.
setup_ws "$TMP/c" 20000 "early days"
run_arm "$TMP/c"
[[ ! -f "$TMP/c/.fno/.handoff-armed-$SID" ]] && pass "no arm below threshold" \
  || fail "armed below threshold"

# 4. Done sentinel present -> no arm.
setup_ws "$TMP/d" 160000 "working"
touch "$TMP/d/.fno/.handoff-done-$SID"
run_arm "$TMP/d"
[[ ! -f "$TMP/d/.fno/.handoff-armed-$SID" ]] && pass "no arm when handoff already done" \
  || fail "armed despite done sentinel"

# 5. Unreadable transcript -> decline (no false arm), exit 0.
setup_ws "$TMP/e" 160000 "working"
( cd "$TMP/e" && printf '{"transcript_path":"/nonexistent/transcript.jsonl"}' | bash "$ARM" ); RC=$?
[[ $RC -eq 0 ]] && pass "unreadable transcript exits 0" || fail "unreadable transcript rc=$RC"
[[ ! -f "$TMP/e/.fno/.handoff-armed-$SID" ]] && pass "unreadable transcript -> no arm" \
  || fail "armed on unreadable transcript"

# 6. Dead owner pid -> no arm (stale state).
setup_ws "$TMP/f" 160000 "working"
sed -i.bak "s/^owner_pid: .*/owner_pid: 999999/" "$TMP/f/.fno/target-state.md" && rm -f "$TMP/f/.fno/target-state.md.bak"
run_arm "$TMP/f"
[[ ! -f "$TMP/f/.fno/.handoff-armed-$SID" ]] && pass "no arm when owner pid is dead" \
  || fail "armed on stale (dead-owner) state"

# 7. PostCompact re-surfaces an armed marker (even though target_is_active is
#    false for the statusless manifest).
setup_ws "$TMP/g" 160000 "working"
run_arm "$TMP/g"
OUT="$( cd "$TMP/g" && printf '{"session_id":"%s"}' "$SID" | bash "$REINJECT" 2>/dev/null )"
echo "$OUT" | grep -q "Handoff armed" && pass "PostCompact re-surfaces the armed marker" \
  || fail "re-surface missing: $OUT"

# 8. handoff done -> marker cleared: PostCompact stops nudging.
touch "$TMP/g/.fno/.handoff-done-$SID"; rm -f "$TMP/g/.fno/.handoff-armed-$SID"
OUT="$( cd "$TMP/g" && printf '{"session_id":"%s"}' "$SID" | bash "$REINJECT" 2>/dev/null )"
echo "$OUT" | grep -q "Handoff armed" && fail "still nudging after marker cleared: $OUT" \
  || pass "no nudge once marker cleared"

echo ""
echo "arm-handoff: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
