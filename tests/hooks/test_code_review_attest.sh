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
# plus the negative half, which matters more: every non-review and every
# unrecognized shape must emit nothing AND never call the classifier, so the
# gate holds rather than clearing on evidence that never arrived.
#
# One live specimen is deliberately in the NEGATIVE half: a real fork ended
# with the marker, a blank line, then a sentence naming the file it skipped.
# A review that excluded the only file in the diff read nothing, so its text
# appears below as the byte-exact fixture of a must-stay-silent case. The
# bare-marker protocol shape is the positive; the marker plus trailing prose
# is not.
#
# The hook is driven with FNO pointed at a stub that records `doctor event
# emit` argv AND serves `do review classify` through the REAL classifier from
# the repo tree, writing a marker file on every classify call. The marker is
# how "the classifier never ran" stays distinguishable from "the classifier
# ran and said clean" (AC3-INV): an absence of emits alone cannot tell the
# two apart.

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

if ! command -v jq >/dev/null 2>&1 || ! command -v python3 >/dev/null 2>&1; then
  # The runner has no skip state: it scores 77 as a red failure and a 0 as a
  # green pass. A machine without jq or python3 is broken for this suite,
  # so red is the honest outcome; a 0 would report every case green having
  # asserted nothing.
  echo "SKIP: jq or python3 not available"; exit 77
fi

# --- a real git repo, because emit-attestation.sh head-pins with rev-parse.
# The emitter also measures the diff under review and refuses a zero-line one,
# so the fixture carries a base commit on origin/main plus a feature commit
# with a real change - without the second commit every positive case below
# hits the zero-line refusal and attests nothing.
WORK="$TMP/repo"
mkdir -p "$WORK"
git -C "$WORK" init -q 2>/dev/null
git -C "$WORK" config user.email t@t.t
git -C "$WORK" config user.name t
echo hi > "$WORK/a.txt"
git -C "$WORK" add a.txt
git -C "$WORK" commit -qm init
git -C "$WORK" update-ref refs/remotes/origin/main "$(git -C "$WORK" rev-parse HEAD)"
echo more >> "$WORK/a.txt"
git -C "$WORK" add a.txt
git -C "$WORK" commit -qm feature

# --- stub `fno`: records `doctor event emit` argv in a file this test owns,
# and serves `do review classify` through the real repo-tree classifier,
# writing a marker so a "never classified" assertion has its positive control.
BIN="$TMP/bin"
mkdir -p "$BIN"
EMITTED="$TMP/emitted.jsonl"
CLASSIFY_MARKER="$TMP/classify.ran"
export CLASSIFY_PYTHONPATH="$REPO_ROOT/cli/src"
export CLASSIFY_PYTHON="$REPO_ROOT/cli/.venv/bin/python"
cat > "$BIN/fno-stub" <<STUB
#!/usr/bin/env bash
if [[ "\${1:-}" == "doctor" && "\${2:-}" == "event" && "\${3:-}" == "emit" ]]; then
  printf '%s\n' "\$*" >> "$EMITTED"
  exit 0
fi
if [[ "\${1:-}" == "do" && "\${2:-}" == "review" && "\${3:-}" == "classify" ]]; then
  f=""; shift 3
  while [[ \$# -gt 0 ]]; do
    case "\$1" in
      --findings-file) f="\$2"; shift 2 ;;
      *) shift ;;
    esac
  done
  printf '%s\n' "\$f" >> "$CLASSIFY_MARKER"
  PYTHONPATH="\$CLASSIFY_PYTHONPATH" "\$CLASSIFY_PYTHON" - "\$f" <<'PY'
import json, sys
from fno.review.cli import build_emit_record, RecordBuildError
with open(sys.argv[1], encoding="utf-8") as fh:
    payload = json.load(fh)
try:
    print(json.dumps(build_emit_record(payload)))
except RecordBuildError as exc:
    print(f"classify: {exc}", file=sys.stderr)
    raise SystemExit(2)
PY
  exit \$?
fi
exit 0
STUB
chmod +x "$BIN/fno-stub"

HOOK_STDOUT="$TMP/hook-stdout.txt"

run_hook() {
  # $1 = payload JSON on stdin
  : > "$EMITTED"
  : > "$CLASSIFY_MARKER"
  : > "$HOOK_STDOUT"
  printf '%s' "$1" | FNO="$BIN/fno-stub" bash "$HOOK" >"$HOOK_STDOUT" 2>/dev/null
}

attested() { [[ -s "$EMITTED" ]]; }
classified() { [[ -s "$CLASSIFY_MARKER" ]]; }

# Assert an emit happened with a given verdict. $1 = case label, $2 = pass|fail.
expect_attest_verdict() {
  if attested; then
    if grep -q "\"verdict\":\"$2" "$EMITTED"; then
      pass "$1: attested $2"
    else
      fail "$1: attested with a verdict other than $2: $(cat "$EMITTED")"
    fi
  else
    fail "$1: NO attestation emitted"
  fi
}

# Assert nothing was emitted. $1 = case label.
expect_silent() {
  if attested; then fail "$1: attested (must not)"; else pass "$1: silent"; fi
}

# Assert nothing was emitted AND the classifier never ran (AC3-INV: the
# marker file is the positive control that separates "never classified" from
# "classified zero"). $1 = case label.
expect_silent_noclassify() {
  if attested; then fail "$1: attested (must not)"; return; fi
  if classified; then fail "$1: silent but the classifier ran"; else pass "$1: silent, classifier never ran"; fi
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

codex_item_completed() {
  local turn="$1" review_output="$2"
  jq -nc --arg turn "$turn" --argjson output "$review_output" \
    '{type:"event_msg", payload:{type:"item_completed", turn_id:$turn,
      item:{type:"ExitedReviewMode", review_output:$output}}}'
}

codex_exited_review_mode() {
  local turn="$1" review_output="$2"
  jq -nc --arg turn "$turn" --argjson output "$review_output" \
    '{type:"event_msg", payload:{type:"exited_review_mode", turn_id:$turn,
      review_output:$output}}'
}

codex_stop() {
  local transcript="$1" turn="$2" message="${3-}"
  local dir="$TMP/codex"
  rm -rf "$dir"; mkdir -p "$dir"
  local tpath="$dir/turn.jsonl"
  printf '%s\n' "$transcript" > "$tpath"
  jq -nc --arg cwd "$WORK" --arg turn "$turn" --arg tp "$tpath" --arg msg "$message" \
    '{hook_event_name:"Stop", cwd:$cwd, turn_id:$turn, transcript_path:$tp,
      last_assistant_message:$msg}'
}

codex_stop_unreadable() {
  local turn="$1"
  jq -nc --arg cwd "$WORK" --arg turn "$turn" \
    '{hook_event_name:"Stop", cwd:$cwd, turn_id:$turn,
      transcript_path:"'$TMP'/codex/missing.jsonl", last_assistant_message:"no findings"}'
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

# A subagent whose ONLY code-review trace is a name inside .meta.json. The
# census that closed the meta fallback: every live sidecar whose name
# matched the reviewer regex also carried the forked-skill marker, so this
# shape is a forge attempt, not a fallback the harness produces.
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

echo "== PostToolUse(ReportFindings) =="
run_hook "$(post_tool_use ReportFindings '{"findings":[]}')"
expect_attest_verdict "reportfindings-empty" pass

run_hook "$(post_tool_use ReportFindings '{"findings":[{"file":"a.py","summary":"s","failure_scenario":"f"}]}')"
expect_attest_verdict "reportfindings-nonempty" fail

run_hook "$(post_tool_use ReportFindings '{}')"
expect_silent_noclassify "reportfindings-absent-key"

run_hook "$(post_tool_use Bash '{"command":"ls"}')"
expect_silent_noclassify "posttooluse-other-tool"

echo "== PostToolUse(ReportFindings): the classified record (AC3-HP) =="
AC3_PAYLOAD='{"findings":[
  {"category":"nit","file":"a.py","line":10,"summary":"stale comment","failure_scenario":"none; a reader is misled for one line"},
  {"category":"correctness","file":"b.py","line":20,"summary":"off-by-one","failure_scenario":"wrong total on empty input"}
]}'
run_hook "$(post_tool_use ReportFindings "$AC3_PAYLOAD")"
expect_attest_verdict "ac3hp-one-finding-each-class" fail
grep -q '"findings_blocking":1' "$EMITTED" \
  && pass "ac3hp event carries findings_blocking:1" \
  || fail "ac3hp findings_blocking: $(cat "$EMITTED")"
grep -q '"findings_nonblocking":1' "$EMITTED" \
  && pass "ac3hp event carries findings_nonblocking:1" \
  || fail "ac3hp findings_nonblocking: $(cat "$EMITTED")"
[[ "$(grep -c . "$EMITTED" || true)" == "1" ]] \
  && pass "ac3hp exactly one attestation emitted" \
  || fail "ac3hp emitted $(grep -c . "$EMITTED" || true) lines"
grep -q 'classified 2 finding(s): 1 blocking, 1 non-blocking' "$HOOK_STDOUT" \
  && pass "ac3hp stdout carries the classification line" \
  || fail "ac3hp classification line missing: $(cat "$HOOK_STDOUT")"

echo "== Codex Stop: exact-turn structured review evidence =="
CODEX_TURN="turn-clean"
CODEX_CLEAN_ITEM="$(codex_item_completed "$CODEX_TURN" '{"findings":[]}')"
run_hook "$(codex_stop "$CODEX_CLEAN_ITEM" "$CODEX_TURN" "no findings")"
expect_attest_verdict "codex-stop-empty-findings" pass

CODEX_DIRECT_CLEAN="$(codex_exited_review_mode "$CODEX_TURN" '{"findings":[]}')"
run_hook "$(codex_stop "$CODEX_DIRECT_CLEAN" "$CODEX_TURN" "no findings")"
expect_attest_verdict "codex-stop-direct-empty-findings" pass

CODEX_DIRTY_ITEM="$(codex_item_completed "$CODEX_TURN" '{"findings":[{"file":"a.py","summary":"boom"}]}')"
run_hook "$(codex_stop "$CODEX_DIRTY_ITEM" "$CODEX_TURN" "no findings")"
expect_attest_verdict "codex-stop-nonempty-findings" fail

CODEX_DIRECT_DIRTY="$(codex_exited_review_mode "$CODEX_TURN" '{"findings":[{"file":"a.py","summary":"boom"}]}')"
run_hook "$(codex_stop "$CODEX_DIRECT_DIRTY" "$CODEX_TURN" "no findings")"
expect_attest_verdict "codex-stop-direct-nonempty-findings" fail

CODEX_NULL_ITEM="$(codex_item_completed "$CODEX_TURN" 'null')"
run_hook "$(codex_stop "$CODEX_NULL_ITEM" "$CODEX_TURN" "no findings")"
expect_silent_noclassify "codex-stop-null-review-output"

CODEX_MISSING_FINDINGS="$(jq -nc --arg turn "$CODEX_TURN" \
  '{type:"event_msg",payload:{type:"item_completed",turn_id:$turn,
    item:{type:"ExitedReviewMode",review_output:{}}}}')"
run_hook "$(codex_stop "$CODEX_MISSING_FINDINGS" "$CODEX_TURN" "no findings")"
expect_silent_noclassify "codex-stop-missing-findings"

CODEX_WRONG_TURN="$(codex_item_completed "turn-other" '{"findings":[]}')"
run_hook "$(codex_stop "$CODEX_WRONG_TURN" "$CODEX_TURN" "no findings")"
expect_silent_noclassify "codex-stop-wrong-turn"

CODEX_DUPLICATE="$CODEX_CLEAN_ITEM
$CODEX_CLEAN_ITEM"
run_hook "$(codex_stop "$CODEX_DUPLICATE" "$CODEX_TURN" "no findings")"
expect_silent "codex-stop-duplicate-completion"

run_hook "$(codex_stop_unreadable "$CODEX_TURN")"
expect_silent "codex-stop-unreadable-transcript"

CODEX_MALFORMED="$CODEX_CLEAN_ITEM
not json"
run_hook "$(codex_stop "$CODEX_MALFORMED" "$CODEX_TURN" "no findings")"
expect_silent "codex-stop-malformed-transcript"

run_hook "$(jq -nc --arg cwd "$WORK" --arg turn "$CODEX_TURN" \
  '{hook_event_name:"Stop",cwd:$cwd,turn_id:$turn,
    last_assistant_message:"no findings"}')"
expect_silent "codex-stop-prose-without-structured-review"

# --- THE VERDICT CORPUS (tests/hooks/fixtures/code-review-attest) ----------
# One byte-exact file per measured final-text shape, the ruled verdict encoded
# in the filename suffix (.attest / .silent). The walker below drives the hook
# over every file, so the suite is pinned to the MEASURED PAYLOADS rather than
# to the predicate: flipping the hook's rule without re-measuring the corpus
# goes red here (the negative control: flip the rule, watch this walk go red,
# revert). The payloads live in files because a retyped copy drifts - the live
# specimen's em dash reached an earlier inline copy as a hyphen and the suite
# stayed green on the wrong bytes.
FIXTURES="$REPO_ROOT/tests/hooks/fixtures/code-review-attest"
CORPUS_DIR="$TMP/corpus"
mkdir -p "$CORPUS_DIR"
CORPUS_TX="$CORPUS_DIR/agent-c0rpu5.jsonl"
: > "$CORPUS_TX"
jq -nc '{forkedSkill:true, skillName:"code-review"}' \
  > "$CORPUS_DIR/agent-c0rpu5.forked-skill.marker.json"

echo "== The verdict corpus: every measured shape, ruled by filename =="
for fixture in "$FIXTURES"/*.attest "$FIXTURES"/*.silent; do
  [[ -f "$fixture" ]] || continue
  name="$(basename "$fixture")"
  payload="$(jq -nc --arg cwd "$WORK" --arg tp "$CORPUS_TX" --rawfile msg "$fixture" \
    '{hook_event_name:"SubagentStop", cwd:$cwd, agent_type:"general-purpose",
      agent_id:"c0rpu5", agent_transcript_path:$tp, last_assistant_message:$msg}')"
  run_hook "$payload"
  if [[ "$name" == *.attest ]]; then
    if attested; then pass "corpus $name: attested"; else fail "corpus $name: NO attestation (ruled attest)"; fi
  else
    if attested; then fail "corpus $name: attested (ruled silent)"; else pass "corpus $name: silent"; fi
  fi
done

echo "== SubagentStop: the header identity lane (payload from the corpus) =="
run_hook "$(subagent_stop "" "## Review findings"$'\n\n'"$(<"$FIXTURES/headered-json-clean.attest")")"
expect_attest_verdict "header-opens-message" pass

echo "== SubagentStop: caller-chosen names never identify a review =="
# The one name the harness controls: agent_type naming the skill type. This
# is signal 1's positive case, so a regression in its loop cannot hide
# behind a suite that never sets the field.
run_hook "$(jq -nc --arg cwd "$WORK" --arg msg "$JSON_CLEAN" \
  '{hook_event_name:"SubagentStop", cwd:$cwd, agent_type:"code-review",
    last_assistant_message:$msg}')"
expect_attest_verdict "agent-type-documented-json-clean" pass

# agent_name is the spawn name the caller picked. Naming a task code-review
# does not make its output a review, whatever the output looks like.
run_hook "$(subagent_stop "/code-review" "$JSON_CLEAN")"
expect_silent_noclassify "agent-name-spawn-name-json-clean"

run_hook "$(subagent_stop "code-review high" "$JSON_CLEAN")"
expect_silent_noclassify "agent-name-with-level-json-clean"

# The low-level protocol's bare marker under a caller-chosen name: the
# verdict shape is right, the identity is not.
run_hook "$(subagent_stop "/code-review <level>" "(none)")"
expect_silent_noclassify "agent-name-none-marker"

# Prose around the marker is NOT the marker. The observed shape is the whole
# final text equal to "(none)"; anything longer must never clear the gate,
# an excuse line above the marker least of all. These carry a REAL marker
# sidecar, so it is the verdict that stays silent, not the identity.
echo "== SubagentStop: the forked-skill shape measured live =="
# This is the exact payload that produced six unmergeable PRs. Nothing in it
# names code-review except the sidecar.
run_hook "$(forked_meta_stop "(none)" "/code-review <level>")"
expect_silent_noclassify "meta-spawn-name-none"

# A fork of some OTHER skill must never clear the gate, whatever it printed.
run_hook "$(forked_skill_stop "(none)" "brainstorming")"
expect_silent_noclassify "forked-marker-other-skill"

# No sidecar on disk (renamed, or a harness that writes none) leaves the
# structural signal at 0. Silence, not a guess.
run_hook "$(forked_skill_stop "(none)" "")"
expect_silent_noclassify "forked-no-sidecar"

# skillName alone is not a skill fork. The forkedSkill flag is the part only
# the harness writes; a marker without it gates nothing.
run_hook "$(forked_skill_stop "$JSON_CLEAN" "code-review" "noflag")"
expect_silent_noclassify "marker-without-forkedskill-flag"

# A spawned TASK is not a review. .meta.json exists for every subagent and
# its description is caller prose; no marker, no name field, so prose naming
# the verb must not attest even over an empty fence.
run_hook "$(task_spawn_stop "code-review the failing tests and report matches" $'Matches for the pattern:\n\n```json\n[]\n```')"
expect_silent_noclassify "task-spawn-verb-in-description"

# Same trap in the PAYLOAD: its description field is caller prose too, so a
# task described as a review that ends clean must stay silent. (The
# subagent_stop helper writes agent_name, a field that IS read, so this
# payload is built directly with description.)
run_hook "$(jq -nc --arg cwd "$WORK" \
  --arg desc "code-review the failing tests and report matches" --arg msg "$JSON_CLEAN" \
  '{hook_event_name:"SubagentStop", cwd:$cwd, agent_type:"general-purpose",
    description:$desc, last_assistant_message:$msg}')"
expect_silent_noclassify "task-payload-description-prose"

echo "== SubagentStop: shapes that must stay silent =="
# An unrelated subagent that happens to end in an empty json array must not
# clear a merge gate. Whatever identifies a code-review must be positive.
run_hook "$(subagent_stop "general-purpose" $'Here is the list.\n\n```json\n[]\n```')"
expect_silent_noclassify "unrelated-subagent-empty-array"

# The heading must OPEN the message. Quoting it inside longer output is not
# the review's shape.
run_hook "$(subagent_stop "" $'Notes on the skill:\n## Review findings\n```json\n[]\n```')"
expect_silent_noclassify "heading-quoted-midtext"

run_hook "$(subagent_stop "general-purpose" "(none)")"
expect_silent_noclassify "unrelated-subagent-none-word"

echo "== unrecognized events =="
run_hook '{"hook_event_name":"SessionStart","cwd":"'"$WORK"'"}'
expect_silent_noclassify "unknown-event"

run_hook 'not json at all'
expect_silent_noclassify "garbage-input"

echo "== AC3-INV: a second non-empty fence never reaches the classifier =="
TWO_FENCES=$'## Review findings\n\n```json\n[]\n```\n\n```json\n[{"file":"a.py","summary":"excluded","failure_scenario":"f"}]\n```'
run_hook "$(forked_skill_stop "$TWO_FENCES" "code-review")"
expect_silent_noclassify "second-fence-nonempty"

echo ""
echo "PASS=$PASS FAIL=$FAIL"
[[ $FAIL -eq 0 ]]
