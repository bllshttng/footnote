#!/usr/bin/env bash
# test_capture_plan_mode.sh - verify hooks/capture-plan-mode.sh writes the
# .fno/.pending-plan.md sidecar correctly from a simulated ExitPlanMode
# PostToolUse payload, and degrades non-fatally on every error path.
#
# Covers: AC1-HP (sidecar exists), AC2-FR (native body verbatim),
# Boundaries (empty/whitespace -> no sidecar; ~50KB plan not truncated),
# Errors (write failure logged to hook-events.jsonl, never fatal),
# awaiting-leader-approval skip, on-disk plan capture (tool_response.filePath),
# wrong-tool no-op, last-writer-wins overwrite.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/capture-plan-mode.sh"
TMP=$(mktemp -d -t capture-plan-mode.XXXXXX)
trap 'chmod -R u+w "$TMP" 2>/dev/null; rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

if ! command -v jq >/dev/null 2>&1; then
  echo "SKIP: jq not available"; exit 0
fi

SIDECAR="$TMP/.fno/.pending-plan.md"
EVENTS="$TMP/.fno/hook-events.jsonl"

# Build a PostToolUse hook-input JSON. Args: plan_text [tool_name] [tool_response_json]
mk_input() {
  local plan="$1"
  local tool="${2:-ExitPlanMode}"
  local resp="${3:-null}"
  jq -nc \
    --arg cwd "$TMP" \
    --arg sid "sess-abc-123" \
    --arg tool "$tool" \
    --arg plan "$plan" \
    --argjson resp "$resp" \
    '{hook_event_name:"PostToolUse", tool_name:$tool, cwd:$cwd, session_id:$sid, tool_input:{plan:$plan}, tool_response:$resp}'
}

run_hook() { bash "$HOOK"; }

# --- 1. Happy path: sidecar written with correct frontmatter + verbatim body ---
rm -f "$SIDECAR" "$EVENTS"
PLAN_BODY=$'# Add CSV export\n\nLet users export the table to CSV.\n\n- button in toolbar\n- streams rows'
echo "$(mk_input "$PLAN_BODY")" | run_hook
RC=$?
[[ $RC -eq 0 ]] && pass "happy: hook exits 0" || fail "happy: hook exit code $RC (expected 0)"
[[ -f "$SIDECAR" ]] && pass "happy: sidecar written" || fail "happy: sidecar missing"

if [[ -f "$SIDECAR" ]]; then
  grep -q '^source: claude-plan-mode$' "$SIDECAR" && pass "happy: source field" || fail "happy: source field wrong/missing"
  grep -q '^status: pending$' "$SIDECAR" && pass "happy: status pending" || fail "happy: status not pending"
  grep -q '^session_id: sess-abc-123$' "$SIDECAR" && pass "happy: session_id propagated" || fail "happy: session_id wrong"
  grep -qE '^captured_at: [0-9]{4}-[0-9]{2}-[0-9]{2}T' "$SIDECAR" && pass "happy: captured_at ISO8601" || fail "happy: captured_at missing/malformed"
  grep -q '^slug: add-csv-export$' "$SIDECAR" && pass "happy: slug from heading" || fail "happy: slug not derived from heading ($(grep '^slug:' "$SIDECAR"))"
fi

# --- 2. Native body preserved verbatim (extract body after the frontmatter) ---
if [[ -f "$SIDECAR" ]]; then
  # Body = lines after the 2nd '---'; drop the single separator blank line.
  # ($() strips the trailing newline printf added, matching PLAN_BODY.)
  BODY="$(awk 'c>=2{print} /^---$/{c++}' "$SIDECAR" | sed '1d')"
  if [[ "$BODY" == "$PLAN_BODY" ]]; then
    pass "verbatim: body matches input byte-for-byte"
  else
    fail "verbatim: body differs from input"
    printf 'got:\n%s\n---expected---\n%s\n' "$BODY" "$PLAN_BODY" >&2
  fi
fi

# --- 3. Empty / whitespace-only plan -> no sidecar + skipped logged ---
rm -f "$SIDECAR" "$EVENTS"
echo "$(mk_input $'   \n\t  ')" | run_hook
[[ ! -f "$SIDECAR" ]] && pass "empty: no sidecar for whitespace plan" || fail "empty: sidecar written for whitespace plan"
grep -q 'plan_mode_capture_skipped' "$EVENTS" 2>/dev/null && pass "empty: skip logged" || fail "empty: skip not logged"

# --- 4. Teammate path (awaitingLeaderApproval:true) -> no sidecar ---
# Source-confirmed (ab-588650c7): the Output has NO approved/decision/isError.
# The one real "not approved yet" signal is the team-lead submission path; the
# plan is submitted but not approved, so it must not be captured as pending.
rm -f "$SIDECAR" "$EVENTS"
echo "$(mk_input "# Some plan" "ExitPlanMode" '{"awaitingLeaderApproval":true}')" | run_hook
[[ ! -f "$SIDECAR" ]] && pass "awaiting: no sidecar on awaitingLeaderApproval:true" || fail "awaiting: sidecar written despite awaitingLeaderApproval:true"
grep -q 'awaiting_leader_approval' "$EVENTS" 2>/dev/null && pass "awaiting: skip logged" || fail "awaiting: skip not logged"

# --- 4a. Legacy phantom fields are NOT a skip signal (they never existed) ---
# A PostToolUse fire already means approval; approved/decision/isError are not
# real Output fields, so a payload carrying them must still WRITE (the old
# guard was vacuous). Guards the semantic change in the skip gate.
rm -f "$SIDECAR" "$EVENTS"
echo "$(mk_input "# Plan despite phantom fields" "ExitPlanMode" '{"approved":false,"decision":"reject","isError":true}')" | run_hook
[[ -f "$SIDECAR" ]] && pass "phantom: writes despite legacy approved/decision/isError" || fail "phantom: skipped on non-existent fields (vacuous-guard regression)"

# --- 4b. Non-object tool_response (string) -> still writes (safe default) ---
rm -f "$SIDECAR" "$EVENTS"
echo "$(mk_input "# Plan with string response" "ExitPlanMode" '"some string"')" | run_hook
[[ -f "$SIDECAR" ]] && pass "string-resp: writes when tool_response is a scalar" || fail "string-resp: did not write (over-aggressive reject gate)"

# --- 5. Wrong tool_name -> no-op ---
rm -f "$SIDECAR" "$EVENTS"
echo "$(mk_input "# Not a plan" "Bash")" | run_hook
[[ ! -f "$SIDECAR" ]] && pass "wrong-tool: no sidecar for non-ExitPlanMode" || fail "wrong-tool: sidecar written for Bash"

# --- 6. Write failure -> logged + non-fatal exit 0 ---
# Make .fno read-only so creating the tmp sidecar (a NEW file) fails,
# but pre-create hook-events.jsonl as a writable file so the failure log
# (an APPEND to an existing file) still succeeds - appending needs write on
# the FILE, not the dir. This isolates "sidecar write fails, failure logged".
# Root (common in CI/Docker) bypasses POSIX dir-write perms, so `chmod a-w`
# wouldn't block the write and this case would spuriously fail. Skip as root.
if [[ $(id -u) -ne 0 ]]; then
  rm -rf "$TMP/.fno"
  mkdir -p "$TMP/.fno"
  : > "$EVENTS"                 # existing, writable events file
  chmod a-w "$TMP/.fno"   # read-only dir: new-file creation blocked
  echo "$(mk_input "# Plan that cannot be written")" | run_hook
  RC=$?
  chmod u+w "$TMP/.fno"   # restore so later cases + cleanup work
  [[ $RC -eq 0 ]] && pass "write-fail: hook still exits 0 (non-fatal)" || fail "write-fail: hook exited $RC"
  [[ ! -f "$SIDECAR" ]] && pass "write-fail: no sidecar produced" || fail "write-fail: sidecar unexpectedly written"
  grep -q 'plan_mode_capture_failed' "$EVENTS" 2>/dev/null && pass "write-fail: failure logged" || fail "write-fail: failure not logged"
else
  pass "write-fail: skipped (root bypasses directory write permissions)"
fi

# --- 7. Last-writer-wins: second capture overwrites the first ---
rm -f "$SIDECAR" "$EVENTS"
echo "$(mk_input "# First plan")" | run_hook
echo "$(mk_input "# Second plan")" | run_hook
if [[ -f "$SIDECAR" ]]; then
  grep -q 'slug: second-plan' "$SIDECAR" && pass "last-writer: second plan won" || fail "last-writer: did not overwrite ($(grep '^slug:' "$SIDECAR"))"
  COUNT=$(grep -c '^source: claude-plan-mode$' "$SIDECAR")
  [[ "$COUNT" -eq 1 ]] && pass "last-writer: exactly one frontmatter block" || fail "last-writer: $COUNT source lines (expected 1)"
fi

# --- 8. Large (~50KB) plan not truncated ---
rm -f "$SIDECAR" "$EVENTS"
# awk BEGIN loop: portable (no GNU/BSD `head -c`), and far cheaper than a
# 51200-element brace expansion.
BIG="# Big plan"$'\n\n'"$(awk 'BEGIN{for(i=0;i<51200;i++)printf "x"}')"
echo "$(mk_input "$BIG")" | run_hook
if [[ -f "$SIDECAR" ]]; then
  XCOUNT=$(grep -o 'x' "$SIDECAR" | wc -l | tr -d ' ')
  [[ "$XCOUNT" -ge 51200 ]] && pass "large: ~50KB body not truncated ($XCOUNT x's)" || fail "large: body truncated ($XCOUNT x's, expected >=51200)"
fi

# --- 9. Inline tool_response.plan is read when no disk path is present ---
rm -f "$SIDECAR" "$EVENTS"
jq -nc --arg cwd "$TMP" --arg sid "sess-resp" \
  '{tool_name:"ExitPlanMode", cwd:$cwd, session_id:$sid, tool_input:{}, tool_response:{plan:"# Plan from response\n\nbody"}}' \
  | run_hook
if [[ -f "$SIDECAR" ]]; then
  grep -q 'slug: plan-from-response' "$SIDECAR" && pass "tool_response.plan: read as inline fallback" || fail "tool_response.plan: not read ($(grep '^slug:' "$SIDECAR"))"
else
  fail "tool_response.plan: no sidecar written from response-only payload"
fi

# --- 9b. (ab-588650c7) plan body read from tool_response.filePath, inline null ---
# The V2 ExitPlanMode tool saves the plan to disk; the real approved Output is
# {plan:null, isAgent, filePath, hasTaskTool} (confirmed by live capture). The
# hook must read the file, or a genuine approved plan is silently missed.
rm -f "$SIDECAR" "$EVENTS"
PLANF="$TMP/plan-on-disk.md"
printf '%s\n' "# Plan on disk" "" "body from the saved file" > "$PLANF"
jq -nc --arg cwd "$TMP" --arg sid "sess-file" --arg pf "$PLANF" \
  '{tool_name:"ExitPlanMode", cwd:$cwd, session_id:$sid, tool_input:{}, tool_response:{plan:null, isAgent:false, filePath:$pf, hasTaskTool:true}}' \
  | run_hook
if [[ -f "$SIDECAR" ]]; then
  grep -q 'slug: plan-on-disk' "$SIDECAR" && pass "filePath: plan read from tool_response.filePath" || fail "filePath: slug not from disk plan ($(grep '^slug:' "$SIDECAR"))"
  grep -q 'body from the saved file' "$SIDECAR" && pass "filePath: disk body captured verbatim" || fail "filePath: disk body missing from sidecar"
else
  fail "filePath: no sidecar from filePath-only payload (the capture-miss bug)"
fi

# --- 9c. (ab-588650c7) tool_input.planFilePath is the secondary disk source ---
rm -f "$SIDECAR" "$EVENTS"
PLANF2="$TMP/plan-input-path.md"
printf '%s\n' "# Plan via input path" "" "ipath body" > "$PLANF2"
jq -nc --arg cwd "$TMP" --arg sid "sess-ipath" --arg pf "$PLANF2" \
  '{tool_name:"ExitPlanMode", cwd:$cwd, session_id:$sid, tool_input:{planFilePath:$pf}, tool_response:{plan:null}}' \
  | run_hook
if [[ -f "$SIDECAR" ]] && grep -q 'slug: plan-via-input-path' "$SIDECAR"; then
  pass "planFilePath: read from tool_input.planFilePath"
else
  fail "planFilePath: not read ($([[ -f "$SIDECAR" ]] && grep '^slug:' "$SIDECAR" || echo no-sidecar))"
fi

# --- 9d. (ab-588650c7) unreadable filePath falls back to inline plan ---
# A stale/deleted filePath must not strand the capture: fall through to inline.
rm -f "$SIDECAR" "$EVENTS"
jq -nc --arg cwd "$TMP" --arg sid "sess-fallback" \
  '{tool_name:"ExitPlanMode", cwd:$cwd, session_id:$sid, tool_input:{plan:"# Inline fallback plan\n\nbody"}, tool_response:{plan:null, filePath:"/nonexistent/does-not-exist.md"}}' \
  | run_hook
if [[ -f "$SIDECAR" ]] && grep -q 'slug: inline-fallback-plan' "$SIDECAR"; then
  pass "filePath-fallback: unreadable path falls back to inline tool_input.plan"
else
  fail "filePath-fallback: did not fall back ($([[ -f "$SIDECAR" ]] && grep '^slug:' "$SIDECAR" || echo no-sidecar))"
fi

# --- 9e. (Gemini) relative filePath resolves against the tool's cwd, not the hook's ---
rm -f "$SIDECAR" "$EVENTS"
printf '%s\n' "# Relative plan" "" "rel body" > "$TMP/relplan.md"
jq -nc --arg cwd "$TMP" --arg sid "sess-rel" \
  '{tool_name:"ExitPlanMode", cwd:$cwd, session_id:$sid, tool_input:{}, tool_response:{plan:null, filePath:"relplan.md"}}' \
  | run_hook
if [[ -f "$SIDECAR" ]] && grep -q 'slug: relative-plan' "$SIDECAR"; then
  pass "relative-filePath: resolved against cwd and captured"
else
  fail "relative-filePath: not resolved ($([[ -f "$SIDECAR" ]] && grep '^slug:' "$SIDECAR" || echo no-sidecar))"
fi

# --- 9f. (Gemini) a directory filePath is skipped (-f), falls back to inline ---
rm -f "$SIDECAR" "$EVENTS"
mkdir -p "$TMP/aplandir"
jq -nc --arg cwd "$TMP" --arg sid "sess-dir" --arg dir "$TMP/aplandir" \
  '{tool_name:"ExitPlanMode", cwd:$cwd, session_id:$sid, tool_input:{plan:"# Dir fallback plan\n\nbody"}, tool_response:{plan:null, filePath:$dir}}' \
  | run_hook
if [[ -f "$SIDECAR" ]] && grep -q 'slug: dir-fallback-plan' "$SIDECAR"; then
  pass "dir-filePath: directory skipped via -f, fell back to inline"
else
  fail "dir-filePath: not handled ($([[ -f "$SIDECAR" ]] && grep '^slug:' "$SIDECAR" || echo no-sidecar))"
fi

# --- 10. (Codex) sidecar lands at REPO ROOT even when cwd is a subdirectory ---
REPO="$TMP/repo"; SUB="$REPO/pkg/sub"
mkdir -p "$SUB"; ( cd "$REPO" && git init -q )
jq -nc --arg cwd "$SUB" --arg sid "sess-sub" --arg plan "# Subdir plan" \
  '{tool_name:"ExitPlanMode", cwd:$cwd, session_id:$sid, tool_input:{plan:$plan}, tool_response:null}' \
  | run_hook
[[ -f "$REPO/.fno/.pending-plan.md" ]] && pass "repo-root: sidecar at <root>/.fno (not the subdir)" || fail "repo-root: sidecar not at repo root"
[[ ! -f "$SUB/.fno/.pending-plan.md" ]] && pass "repo-root: nothing stranded in the subdir" || fail "repo-root: sidecar stranded in subdir"

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
