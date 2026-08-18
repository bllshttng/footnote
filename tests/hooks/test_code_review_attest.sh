#!/usr/bin/env bash
# test_code_review_attest.sh - drive hooks/code-review-attest.sh over the full
# payload matrix a real /code-review pass can arrive in (x-bcb5).
#
# WHY THIS FILE EXISTS. The hook is the sole producer for the
# config.review.reviewers gate on the /code-review path, and the gate became a
# MERGE BLOCKER on 2026-08-16. A producer that fires on one of N delivery
# shapes does not make the gate bypassable; it makes it UNSATISFIABLE. Six PRs
# shipped green-but-unmergeable before anyone noticed, each rescued by a
# hand-run emit-attestation.sh.
#
# The matrix below is the delivery shapes OBSERVED LIVE, not shapes imagined:
#   - ReportFindings tool call with findings: []          (works, keep working)
#   - a fork ending in a fenced json block, WITH the header
#   - a fork ending in a fenced json block, NO header
#   - a fork whose entire final text is the literal "(none)"
# plus the negative half, which matters more: every non-clean and every
# unrecognized shape must emit NOTHING, so the gate holds rather than clearing
# on evidence that never arrived.
#
# The hook is driven with FNO pointed at a stub recorder, so a "did it attest?"
# assertion reads a file this test owns rather than the real event log.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/code-review-attest.sh"
TMP=$(mktemp -d -t code-review-attest.XXXXXX)
trap 'chmod -R u+w "$TMP" 2>/dev/null; rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

if ! command -v jq >/dev/null 2>&1; then
  echo "SKIP: jq not available"; exit 0
fi

# --- a real git repo, because emit-attestation.sh head-pins with rev-parse ---
WORK="$TMP/repo"
mkdir -p "$WORK"
git -C "$WORK" init -q 2>/dev/null
git -C "$WORK" config user.email t@t.t
git -C "$WORK" config user.name t
echo hi > "$WORK/a.txt"
git -C "$WORK" add a.txt
git -C "$WORK" commit -qm init

# --- stub `fno` so an emit lands in a file this test owns ---
BIN="$TMP/bin"
mkdir -p "$BIN"
EMITTED="$TMP/emitted.jsonl"
cat > "$BIN/fno-stub" <<STUB
#!/usr/bin/env bash
# records only \`event emit\` calls; anything else is a silent no-op
if [[ "\${1:-}" == "event" && "\${2:-}" == "emit" ]]; then
  printf '%s\n' "\$*" >> "$EMITTED"
fi
exit 0
STUB
chmod +x "$BIN/fno-stub"

run_hook() {
  # $1 = payload JSON on stdin
  : > "$EMITTED"
  printf '%s' "$1" | FNO="$BIN/fno-stub" bash "$HOOK" >/dev/null 2>&1
}

attested() { [[ -s "$EMITTED" ]]; }

# Assert an emit happened. $1 = case label.
expect_attest() {
  if attested; then pass "$1: attested"; else fail "$1: NO attestation emitted"; fi
}

# Assert nothing was emitted. $1 = case label.
expect_silent() {
  if attested; then fail "$1: attested (must not)"; else pass "$1: silent"; fi
}

post_tool_use() {
  # $1 = tool name, $2 = tool_input JSON
  jq -nc --arg cwd "$WORK" --arg tool "$1" --argjson ti "$2" \
    '{hook_event_name:"PostToolUse", tool_name:$tool, cwd:$cwd, tool_input:$ti}'
}

subagent_stop() {
  # $1 = description (may be empty), $2 = last assistant message
  jq -nc --arg cwd "$WORK" --arg desc "$1" --arg msg "$2" \
    '{hook_event_name:"SubagentStop", cwd:$cwd, agent_name:$desc, last_assistant_message:$msg}'
}

# The shape measured live on 2026-08-17 from a `Skill(skill="code-review")`
# fork: agent_type is the generic "general-purpose", NO name field carries the
# skill, and the only record of what ran is the sidecar the harness writes
# beside agent_transcript_path.
#
# $1 = last assistant message
# $2 = skillName to write into the marker sidecar ("" = write no sidecar)
forked_skill_stop() {
  local msg="$1" skill="${2-}"
  local dir="$TMP/subagents"
  rm -rf "$dir"; mkdir -p "$dir"
  local tpath="$dir/agent-a2b49adb85a7931f6.jsonl"
  : > "$tpath"
  if [[ -n "$skill" ]]; then
    jq -nc --arg s "$skill" '{forkedSkill:true, skillName:$s}' \
      > "$dir/agent-a2b49adb85a7931f6.forked-skill.marker.json"
  fi
  jq -nc --arg cwd "$WORK" --arg msg "$msg" --arg tp "$tpath" \
    '{hook_event_name:"SubagentStop", cwd:$cwd, agent_type:"general-purpose",
      agent_id:"a2b49adb85a7931f6", agent_transcript_path:$tp,
      last_assistant_message:$msg}'
}

# Same fork, but the harness recorded the invocation only in the .meta.json
# sidecar (agentType generic, description carrying the real verb).
forked_meta_stop() {
  local msg="$1" desc="$2"
  local dir="$TMP/subagents"
  rm -rf "$dir"; mkdir -p "$dir"
  local tpath="$dir/agent-deadbeef.jsonl"
  : > "$tpath"
  jq -nc --arg d "$desc" \
    '{agentType:"general-purpose", description:$d, name:"code-review", spawnDepth:1}' \
    > "$dir/agent-deadbeef.meta.json"
  jq -nc --arg cwd "$WORK" --arg msg "$msg" --arg tp "$tpath" \
    '{hook_event_name:"SubagentStop", cwd:$cwd, agent_type:"general-purpose",
      agent_transcript_path:$tp, last_assistant_message:$msg}'
}

JSON_CLEAN=$'Reviewed the diff at HEAD.\n\n```json\n[]\n```\n\nNothing survived verification.'
JSON_DIRTY=$'```json\n[{"file":"a.py","summary":"boom","failure_scenario":"x"}]\n```'

echo "== PostToolUse(ReportFindings) =="
run_hook "$(post_tool_use ReportFindings '{"findings":[]}')"
expect_attest "reportfindings-empty"

run_hook "$(post_tool_use ReportFindings '{"findings":[{"file":"a.py","summary":"s","failure_scenario":"f"}]}')"
expect_silent "reportfindings-nonempty"

run_hook "$(post_tool_use ReportFindings '{}')"
expect_silent "reportfindings-absent-key"

run_hook "$(post_tool_use Bash '{"command":"ls"}')"
expect_silent "posttooluse-other-tool"

echo "== SubagentStop: shapes that must attest =="
run_hook "$(subagent_stop "/code-review" "$JSON_CLEAN")"
expect_attest "described-json-clean"

run_hook "$(subagent_stop "" "## Review findings"$'\n\n'"$JSON_CLEAN")"
expect_attest "headered-json-clean"

# Three flagless forks once returned findings JSON with NO header, and the
# description field carried nothing the matcher recognized. Zero attestations.
run_hook "$(subagent_stop "code-review high" "$JSON_CLEAN")"
expect_attest "described-with-level-json-clean"

# The low-level protocol's empty-findings marker is the literal "(none)" as
# the WHOLE final text. No fence at all, so the fence parser could never
# read it.
run_hook "$(subagent_stop "/code-review <level>" "(none)")"
expect_attest "described-none-marker"

# The measured shape: the marker LEADS, then one sentence explaining that the
# diff held nothing this level reviews. A complete review of an empty scope,
# so it must attest. This exact text is the fork's own final message.
run_hook "$(subagent_stop "/code-review" $'(none)\n\nThe only change is `tests/hooks/test_code_review_attest.sh`, a new test file - excluded from review at this level.')"
expect_attest "none-marker-leads-then-scope-note"

# Prose BEFORE the marker is not a verdict. The first non-blank line decides,
# so an excuse line above the marker attests nothing.
run_hook "$(subagent_stop "/code-review" $'Reviewed.\n\n(none)\n')"
expect_silent "described-none-marker-buried-in-prose"

run_hook "$(subagent_stop "/code-review" $'I could not inspect the diff.\n\n(none)')"
expect_silent "described-none-marker-after-failure"

echo "== SubagentStop: the forked-skill shape measured live =="
# This is the exact payload that produced six unmergeable PRs. Nothing in it
# names code-review except the sidecar.
run_hook "$(forked_skill_stop "(none)" "code-review")"
expect_attest "forked-marker-none"

run_hook "$(forked_skill_stop "$JSON_CLEAN" "code-review")"
expect_attest "forked-marker-json-clean"

run_hook "$(forked_meta_stop "(none)" "/code-review <level>")"
expect_attest "forked-meta-none"

run_hook "$(forked_skill_stop "$JSON_DIRTY" "code-review")"
expect_silent "forked-marker-json-dirty"

# A fork of some OTHER skill must never clear the gate, whatever it printed.
run_hook "$(forked_skill_stop "(none)" "brainstorming")"
expect_silent "forked-marker-other-skill"

# No sidecar on disk (renamed, or a harness that writes none) leaves the
# structural signal at 0. Silence, not a guess.
run_hook "$(forked_skill_stop "(none)" "")"
expect_silent "forked-no-sidecar"

echo "== SubagentStop: shapes that must stay silent =="
run_hook "$(subagent_stop "/code-review" "$JSON_DIRTY")"
expect_silent "described-json-dirty"

run_hook "$(subagent_stop "" "## Review findings"$'\n\n'"$JSON_DIRTY")"
expect_silent "headered-json-dirty"

# A review that produced no parseable verdict at all is NOT a clean pass.
# Absence has two explanations and a producer must never guess between them.
run_hook "$(subagent_stop "/code-review" "I could not read the diff.")"
expect_silent "described-unparseable"

run_hook "$(subagent_stop "/code-review" "")"
expect_silent "described-empty-message"

# An unrelated subagent that happens to end in an empty json array must not
# clear a merge gate. Whatever identifies a code-review must be positive.
run_hook "$(subagent_stop "general-purpose" $'Here is the list.\n\n```json\n[]\n```')"
expect_silent "unrelated-subagent-empty-array"

run_hook "$(subagent_stop "general-purpose" "(none)")"
expect_silent "unrelated-subagent-none-word"

echo "== unrecognized events =="
run_hook '{"hook_event_name":"SessionStart","cwd":"'"$WORK"'"}'
expect_silent "unknown-event"

run_hook 'not json at all'
expect_silent "garbage-input"

echo ""
echo "PASS=$PASS FAIL=$FAIL"
[[ $FAIL -eq 0 ]]
