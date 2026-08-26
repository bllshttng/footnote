#!/usr/bin/env bash
# test_review_hold_hook.sh - drive hooks/review-hold.sh over the payloads a
# real review invocation arrives in.
#
# WHY THIS FILE EXISTS. This hook is the registration site that would have
# caught all three 2026-08-22 specimens: every one of them was a review the
# worker self-invoked through the Skill tool, which is not footnote code and
# cannot register a hold on its own. A hook that misses the payload shape
# registers nothing, and the guard downstream falls back to its worktree layer
# alone - which is blind to the window between "review dispatched" and "first
# edit", the exact window PR 1072 was merged-ready in.
#
# The negative half matters as much. This runs on PreToolUse for EVERY Skill
# call in the session, so a name rule that is too loose takes a hold nobody
# meant to take, and a merge nobody can complete. `code-review-attest` and
# `pr-review-fixes` both contain "review" and must register nothing.
#
# The hook is driven with FNO pointed at a stub recorder, so "did it register?"
# reads a file this test owns rather than the real claims store.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/review-hold.sh"
TMP=$(mktemp -d -t review-hold-hook.XXXXXX)
trap 'chmod -R u+w "$TMP" 2>/dev/null; rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

if ! command -v jq >/dev/null 2>&1; then
  echo "SKIP: jq not available"; exit 77
fi

# --- a real git repo on a feature branch, because the hook reads the branch ---
WORK="$TMP/repo"
mkdir -p "$WORK"
git -C "$WORK" init -q 2>/dev/null
git -C "$WORK" config user.email t@t.t
git -C "$WORK" config user.name t
echo hi > "$WORK/a.txt"
git -C "$WORK" add a.txt
git -C "$WORK" commit -qm init
git -C "$WORK" checkout -q -b feature/x-a089

BIN="$TMP/bin"
mkdir -p "$BIN"
RECORDED="$TMP/recorded.txt"
EVENTS="$TMP/events.jsonl"
cat > "$BIN/fno-stub" <<'STUB'
#!/usr/bin/env bash
if [[ "${1:-}" == "do" && "${2:-}" == "pr" && "${3:-}" == "review-hold" ]]; then
  printf '%s\n' "$*" >> "$FNO_RECORD"
fi
if [[ "${1:-}" == "doctor" && "${2:-}" == "event" && "${3:-}" == "emit" ]]; then
  printf '%s\n' "$*" >> "$FNO_EVENTS"
fi
exit 0
STUB
chmod +x "$BIN/fno-stub"

run_hook() {
  # $1 = action, $2 = payload JSON
  : > "$RECORDED"
  : > "$EVENTS"
  printf '%s' "$2" | FNO="$BIN/fno-stub" FNO_RECORD="$RECORDED" FNO_EVENTS="$EVENTS" \
    bash "$HOOK" "$1" >/dev/null 2>&1
}

registered() { [[ -s "$RECORDED" ]]; }

expect_registered() {
  if registered; then pass "$1: registered"; else fail "$1: NOTHING registered"; fi
}

expect_silent() {
  if registered; then fail "$1: registered (must not)"; else pass "$1: silent"; fi
}

skill_call() {
  # $1 = skill name, $2 = cwd (default $WORK)
  jq -nc --arg cwd "${2:-$WORK}" --arg s "$1" \
    '{hook_event_name:"PreToolUse", tool_name:"Skill", cwd:$cwd,
      session_id:"sess-1", tool_input:{skill:$s}}'
}

echo "-- the review verbs register a hold --"
for verb in code-review review review-changes sigma-review; do
  run_hook acquire "$(skill_call "$verb")"
  expect_registered "skill=$verb"
done

echo "-- plugin-qualified and slash-prefixed spellings are the same verb --"
run_hook acquire "$(skill_call "fno:review")"
expect_registered "skill=fno:review"
run_hook acquire "$(skill_call "/code-review")"
expect_registered "skill=/code-review"
run_hook acquire "$(skill_call "/code-review <level> --comment")"
expect_registered "skill=/code-review with args"

echo "-- a review start records a positive invocation marker and joins the hold --"
review_level="medium"
run_hook acquire "$(jq -nc --arg cwd "$WORK" --arg level "$review_level" \
  '{hook_event_name:"PreToolUse", tool_name:"Skill", cwd:$cwd,
    session_id:"sess-started", tool_input:{skill:("/code-review " + $level + " --comment")}}')"
expect_registered "review start telemetry: hold"
if grep -q 'review_invocation' "$EVENTS"; then
  pass "review start telemetry: invocation event"
else
  fail "review start telemetry: no invocation event"
fi
event_id="$(grep -o 'ri-[0-9a-f]*' "$EVENTS" | head -1)"
hold_id="$(grep -o 'ri-[0-9a-f]*' "$RECORDED" | head -1)"
if [[ -n "$event_id" && "$event_id" == "$hold_id" ]]; then
  pass "review start telemetry: event and hold share invocation id"
else
  fail "review start telemetry: join id mismatch (event=$event_id hold=$hold_id)"
fi
if grep -q 'stage.*started' "$EVENTS" && grep -q 'level.*medium' "$EVENTS" \
  && grep -q 'level_source.*explicit' "$EVENTS" \
  && grep -q 'transport.*skill_tool' "$EVENTS"; then
  pass "review start telemetry: parsed level and transport"
else
  fail "review start telemetry: missing parsed fields ($(cat "$EVENTS"))"
fi

echo "-- a name that merely CONTAINS review registers nothing --"
for verb in code-review-attest pr-review-fixes reviewer brainstorming; do
  run_hook acquire "$(skill_call "$verb")"
  expect_silent "skill=$verb"
done

echo "-- the payload has to be a Skill call --"
run_hook acquire "$(jq -nc --arg cwd "$WORK" \
  '{hook_event_name:"PreToolUse", tool_name:"Bash", cwd:$cwd,
    tool_input:{command:"code-review"}}')"
expect_silent "tool=Bash"

run_hook acquire "$(jq -nc --arg cwd "$WORK" \
  '{hook_event_name:"PreToolUse", tool_name:"Skill", cwd:$cwd, tool_input:{}}')"
expect_silent "skill name absent"

echo "-- a protected branch is not a PR branch --"
git -C "$WORK" checkout -q main 2>/dev/null || git -C "$WORK" checkout -q master
run_hook acquire "$(skill_call "code-review")"
expect_silent "on the protected branch"
git -C "$WORK" checkout -q feature/x-a089

echo "-- outside a repo there is no branch to key on --"
mkdir -p "$TMP/norepo"
run_hook acquire "$(skill_call "code-review" "$TMP/norepo")"
expect_silent "cwd is not a repo"

echo "-- the hook only ever acquires --"
# A PostToolUse release was wired here and removed. For an INLINE skill the
# Skill tool returns the SKILL.md body and the review runs AFTERWARDS, so the
# release fired within milliseconds and the hold covered nothing - precisely
# the window layer 2 cannot see. The release lives at the attestation and the
# TTL instead.
run_hook release "$(skill_call "code-review")"
expect_silent "action=release"

echo "-- acquire pins the head it is reviewing --"
run_hook acquire "$(skill_call "code-review")"
head_sha="$(git -C "$WORK" rev-parse HEAD)"
if grep -q -- "--head $head_sha" "$RECORDED" 2>/dev/null; then
  pass "acquire: recorded the head"
else
  fail "acquire: head not recorded ($(cat "$RECORDED"))"
fi

echo "-- an unknown action does nothing --"
run_hook frobnicate "$(skill_call "code-review")"
expect_silent "action=frobnicate"

echo "-- the hook NEVER blocks: exit 0 and no permission decision --"
out="$(printf '%s' "$(skill_call "code-review")" \
  | FNO="$BIN/fno-stub" FNO_RECORD="$RECORDED" bash "$HOOK" acquire 2>/dev/null)"
rc=$?
if [[ $rc -eq 0 && -z "$out" ]]; then
  pass "silent exit 0"
else
  fail "exit $rc with output: $out"
fi

echo ""
echo "PASS: $PASS  FAIL: $FAIL"
[[ $FAIL -eq 0 ]]
