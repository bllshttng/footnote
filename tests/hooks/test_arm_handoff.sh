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
trap 'FNO_CLAIMS_ROOT="$TMP" fno claim release "node:x-test" >/dev/null 2>&1; rm -rf "$TMP"' EXIT
SID="sess-xyz"

# Hermetic claims root: `fno claim` reads/writes <root>/.fno/claims/.
export FNO_CLAIMS_ROOT="$TMP"
fno claim acquire "node:x-test" --holder "target-session:$SID" --ttl 1h >/dev/null 2>&1 \
  || { echo "FAIL: could not acquire the fixture claim" >&2; exit 1; }

# Build a workspace in the shape PRODUCTION actually produces: a DEAD owner_pid
# plus a live node claim. owner_pid is the transient `fno target init` wrapper
# pid and is dead within ~1s of init returning, so a fixture that writes a live
# `owner_pid: $$` tests a state no real session is ever in - which is how the
# hook shipped gated on `kill -0 owner_pid` and exited on 100% of real fires.
# $1=dir $2=used_tokens $3=last assistant text $4=claim key (default node:x-test,
# pass "" to model a claimless free-text/plan run).
setup_ws() {
  local dir="$1" used_tokens="$2" last_text="$3" claim_key="${4-node:x-test}"
  rm -rf "$dir"; mkdir -p "$dir/.fno"
  cat > "$dir/.fno/target-state.md" <<EOF
---
session_id: $SID
owner_pid: 999999
graph_node_id: x-test
input: x-test
plan_path: /tmp/plan
---
EOF
  [[ -n "$claim_key" ]] && printf 'target_claim_key: "%s"\n' "$claim_key" >> "$dir/.fno/target-state.md"
  # Minimal transcript: one assistant line carrying usage + text.
  printf '{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":%s,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"content":[{"type":"text","text":"%s"}]}}\n' \
    "$used_tokens" "$last_text" > "$dir/transcript.jsonl"
}

run_arm() { # dir  -> runs the hook from within dir with transcript on stdin
  local dir="$1"
  ( cd "$dir" && printf '{"transcript_path":"%s/transcript.jsonl"}' "$dir" | bash "$ARM" )
}

# 1. Pressure (80%) + outstanding work -> arm marker written.
setup_ws "$TMP/a" 800000 "still working on it"
MANIFEST_BEFORE="$(cat "$TMP/a/.fno/target-state.md")"
run_arm "$TMP/a"
[[ -f "$TMP/a/.fno/.handoff-armed-$SID" ]] && pass "arms on pressure + outstanding work" \
  || fail "expected arming marker, none written"
grep -q '"node_id":"x-test"' "$TMP/a/.fno/.handoff-armed-$SID" && pass "marker carries node_id" \
  || fail "marker missing node_id"
[[ "$(cat "$TMP/a/.fno/target-state.md")" == "$MANIFEST_BEFORE" ]] \
  && pass "manifest NOT mutated (invariant)" || fail "manifest was mutated"

# 2. <promise> present -> no arm (session finishing).
setup_ws "$TMP/b" 800000 "<promise>MISSION COMPLETE: done</promise>"
run_arm "$TMP/b"
[[ ! -f "$TMP/b/.fno/.handoff-armed-$SID" ]] && pass "no arm when <promise> present" \
  || fail "armed despite <promise>"

# 3. Below threshold (10%) -> no arm.
setup_ws "$TMP/c" 100000 "early days"
run_arm "$TMP/c"
[[ ! -f "$TMP/c/.fno/.handoff-armed-$SID" ]] && pass "no arm below threshold" \
  || fail "armed below threshold"

# 4. Done sentinel present -> no arm.
setup_ws "$TMP/d" 800000 "working"
touch "$TMP/d/.fno/.handoff-done-$SID"
run_arm "$TMP/d"
[[ ! -f "$TMP/d/.fno/.handoff-armed-$SID" ]] && pass "no arm when handoff already done" \
  || fail "armed despite done sentinel"

# 5. Unreadable transcript -> decline (no false arm), exit 0.
setup_ws "$TMP/e" 800000 "working"
( cd "$TMP/e" && printf '{"transcript_path":"/nonexistent/transcript.jsonl"}' | bash "$ARM" ); RC=$?
[[ $RC -eq 0 ]] && pass "unreadable transcript exits 0" || fail "unreadable transcript rc=$RC"
[[ ! -f "$TMP/e/.fno/.handoff-armed-$SID" ]] && pass "unreadable transcript -> no arm" \
  || fail "armed on unreadable transcript"

# 6. Liveness. owner_pid can only ever PROVE life, never death, so DEATH is
#    asserted from the node claim alone - the same asymmetry
#    cli/src/fno/target/orient.py::_manifest_liveness encodes.
# 6a. Dead owner pid + claim free/absent -> no arm (genuinely stale state).
setup_ws "$TMP/f" 800000 "working" "node:x-not-claimed-zzz"
run_arm "$TMP/f"
[[ ! -f "$TMP/f/.fno/.handoff-armed-$SID" ]] && pass "no arm when the claim is free (stale state)" \
  || fail "armed on stale (free-claim) state"

# 6b. Dead owner pid + LIVE claim -> arm. This is the shape EVERY real session
#     is in, and the one the shipped `kill -0 owner_pid` gate always rejected.
setup_ws "$TMP/f2" 800000 "working"
run_arm "$TMP/f2"
[[ -f "$TMP/f2/.fno/.handoff-armed-$SID" ]] && pass "arms on a dead owner_pid with a LIVE claim" \
  || fail "dead owner_pid + live claim did not arm (the hook is unreachable)"

# 6c. Dead owner pid + NO claim key (free-text / plan run) -> arm. There is no
#     durable death signal, so bias live; a false arm costs one advisory nudge,
#     a false decline costs the whole guard.
setup_ws "$TMP/f3" 800000 "working" ""
run_arm "$TMP/f3"
[[ -f "$TMP/f3/.fno/.handoff-armed-$SID" ]] && pass "arms on a claimless run (biased live)" \
  || fail "claimless run declined; owner_pid was treated as proof of death"

# 6b. Stale <promise> in an EARLIER turn, but the LAST assistant turn has
#     outstanding work -> must still arm (only the last turn counts).
setup_ws "$TMP/p" 800000 "still working, nothing done yet"
# Prepend an older assistant turn that DID carry a promise.
printf '{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":1000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"content":[{"type":"text","text":"<promise>MISSION COMPLETE: earlier subtask</promise>"}]}}\n%s' \
  "$(cat "$TMP/p/transcript.jsonl")" > "$TMP/p/transcript.jsonl.new" && mv "$TMP/p/transcript.jsonl.new" "$TMP/p/transcript.jsonl"
run_arm "$TMP/p"
[[ -f "$TMP/p/.fno/.handoff-armed-$SID" ]] && pass "arms despite a stale <promise> in an earlier turn" \
  || fail "stale earlier <promise> wrongly suppressed arming"

# 7. PostCompact re-surfaces an armed marker (even though target_is_active is
#    false for the statusless manifest).
setup_ws "$TMP/g" 800000 "working"
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
