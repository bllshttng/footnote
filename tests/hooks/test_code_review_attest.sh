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
# One live specimen is deliberately in the NEGATIVE half: a real fork ended
# with the marker, a blank line, then a sentence naming the file it skipped.
# A review that excluded the only file in the diff read nothing, so its text
# appears below as the byte-exact fixture of a must-stay-silent case. The
# bare-marker protocol shape is the positive; the marker plus trailing prose
# is not.
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
  # exit 77 is the tree's skip convention: the smoke runner scores a 77 as a
  # visible skip, while a 0 here would report every case green unexecuted.
  echo "SKIP: jq not available"; exit 77
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
# $3 = "noflag" writes the marker with skillName but WITHOUT forkedSkill
forked_skill_stop() {
  local msg="$1" skill="${2-}" noflag="${3-}"
  local dir="$TMP/subagents"
  rm -rf "$dir"; mkdir -p "$dir"
  local tpath="$dir/agent-a2b49adb85a7931f6.jsonl"
  : > "$tpath"
  if [[ -n "$skill" ]]; then
    if [[ "$noflag" == "noflag" ]]; then
      jq -nc --arg s "$skill" '{skillName:$s}' \
        > "$dir/agent-a2b49adb85a7931f6.forked-skill.marker.json"
    else
      jq -nc --arg s "$skill" '{forkedSkill:true, skillName:$s}' \
        > "$dir/agent-a2b49adb85a7931f6.forked-skill.marker.json"
    fi
  fi
  jq -nc --arg cwd "$WORK" --arg msg "$msg" --arg tp "$tpath" \
    '{hook_event_name:"SubagentStop", cwd:$cwd, agent_type:"general-purpose",
      agent_id:"a2b49adb85a7931f6", agent_transcript_path:$tp,
      last_assistant_message:$msg}'
}

# A plain spawned TASK, not a skill fork: .meta.json exists (it is written for
# every subagent) and its description carries caller prose, but there is no
# forkedSkill marker and no harness-recorded name.
# $1 = spawn description, $2 = last assistant message
task_spawn_stop() {
  local desc="$1" msg="$2"
  local dir="$TMP/subagents"
  rm -rf "$dir"; mkdir -p "$dir"
  local tpath="$dir/agent-b7c31e5a90d24f68.jsonl"
  : > "$tpath"
  jq -nc --arg d "$desc" '{agentType:"fno:type-design-analyzer", description:$d}' \
    > "$dir/agent-b7c31e5a90d24f68.meta.json"
  jq -nc --arg cwd "$WORK" --arg msg "$msg" --arg tp "$tpath" \
    '{hook_event_name:"SubagentStop", cwd:$cwd, agent_type:"general-purpose",
      agent_id:"b7c31e5a90d24f68", agent_transcript_path:$tp,
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

# Prose around the marker is NOT the marker. The observed shape is the whole
# final text equal to "(none)"; anything longer must never clear the gate,
# an excuse line above the marker least of all.
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

# The live specimen, byte exact: the marker, a blank line, then the scope
# sentence naming the file the fork excluded. That review read zero files, so
# strict equality stays silent on its real text, not a paraphrase of it. The
# em-dash inside the sentence is the fork's own byte, kept as evidence.
run_hook "$(forked_skill_stop $'(none)\n\nThe only change is `tests/hooks/test_code_review_attest.sh`, a new test file — excluded from review at this level.' "code-review")"
expect_silent "forked-real-specimen-marker-then-scope-note"

# skillName alone is not a skill fork. The forkedSkill flag is the part only
# the harness writes; a marker without it gates nothing.
run_hook "$(forked_skill_stop "$JSON_CLEAN" "code-review" "noflag")"
expect_silent "marker-without-forkedskill-flag"

# A spawned TASK is not a review. .meta.json exists for every subagent and
# its description is caller prose; no marker, no name field, so prose naming
# the verb must not attest even over an empty fence.
run_hook "$(task_spawn_stop "code-review the failing tests and report matches" $'Matches for the pattern:\n\n```json\n[]\n```')"
expect_silent "task-spawn-verb-in-description"

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

# The heading must OPEN the message. Quoting it inside longer output is not
# the review's shape.
run_hook "$(subagent_stop "" $'Notes on the skill:\n## Review findings\n```json\n[]\n```')"
expect_silent "heading-quoted-midtext"

# When fences carry different arrays, the LAST fence is the verdict: a
# leading quoted [] over a closing non-empty findings block attests nothing.
run_hook "$(subagent_stop "/code-review" $'Spec excerpt:\n```json\n[]\n```\n\n```json\n[{"file":"a.py","summary":"boom","failure_scenario":"x"}]\n```')"
expect_silent "later-fence-carries-findings"

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
