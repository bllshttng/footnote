#!/usr/bin/env bash
# Emit the code-review attestation with the CLASSIFIED finding record.
# Claude and Codex expose different structured completion surfaces, so this
# shared producer handles all reachable paths:
#
#   1. PostToolUse(ReportFindings) - a pass that calls the ReportFindings
#      tool directly, when the active code-review instructions route
#      through it.
#   2. SubagentStop - the Skill-tool self-invocation path
#      (`Skill(skill="code-review", ...)`), which the harness runs as a
#      FORKED subagent. Inside a fork the code-review skill's own
#      instructions can forbid calling ReportFindings ("this review's
#      output contract is the JSON block above" - confirmed live by
#      running this exact PR's own self-review, x-e97b, whose finding
#      caught trigger 1 alone as dead on arrival for that path), so its
#      result surfaces only in the subagent's final text.
#
#   3. Stop - Codex's native `/review` is an internal review task. Its root
#      Stop payload carries the exact turn and transcript path; the transcript
#      carries the structured `ExitedReviewMode` result.
#
# The SubagentStop trigger identifies the fork from the harness's own record of what it
# ran (the sidecar beside `agent_transcript_path`), NOT from the shape of
# what it printed. Keying on printed shape is what left this branch
# decorative through six PRs: see the measurement in the branch itself.
#
# The pass condition is no longer clean-only: a review WITH findings emits
# `fail` carrying the classified record, so a PR whose findings get fixed or
# dispositioned has a durable record to tile instead of reading as never
# reviewed. An empty findings set emits `pass` with an explicit zero-finding
# record - same verdict as before, no longer implicit. Classification is
# `fno do review classify`, the one shell entry point; this hook never
# reimplements the rule in bash. A classify failure (a deployment older
# than the verb) refuses to emit: fail closed, and the stale deployment is
# the thing to fix.
#
# Fail direction: any parse problem or an event this script does not
# recognize emits nothing AND never calls the classifier, so the reviewers
# gate holds rather than clearing on evidence that never arrived.
set -euo pipefail

input="$(cat)"
event="$(printf '%s' "$input" | jq -r '.hook_event_name // empty' 2>/dev/null || true)"

# findings_payload: the JSON the classifier reads ([] for an identified
# clean verdict, the findings array for ReportFindings, the review_output
# object for codex). Empty string means "no review was identified here" -
# emit nothing and call no classifier.
findings_payload=""
record_review_subagent_marker() {
  local cwd branch hold_json invocation_id marker_dir marker_name
  cwd="$(printf '%s' "$input" | jq -r '.cwd // empty' 2>/dev/null || true)"
  [[ -n "$cwd" && -d "$cwd" ]] || return 0
  branch="$(git -C "$cwd" rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
  [[ -n "$branch" && "$branch" != "HEAD" ]] || return 0
  hold_json="$("${FNO:-fno}" do pr review-hold metadata --branch "$branch" \
    --repo "$cwd" 2>/dev/null || true)"
  invocation_id="$(printf '%s' "$hold_json" \
    | jq -r '.metadata.invocation_id // empty' 2>/dev/null || true)"
  [[ -n "$invocation_id" ]] || return 0
  marker_dir="${FNO_HOME:-$HOME/.fno}/review-invocations/${invocation_id}.subagents"
  marker_name="$(printf '%s' "${agent_transcript:-$$}" \
    | shasum -a 256 2>/dev/null | awk '{print $1}')"
  [[ -n "$marker_name" ]] || marker_name="pid-$$"
  mkdir -p "$marker_dir" 2>/dev/null || return 0
  : > "$marker_dir/$marker_name" 2>/dev/null || true
}
reviewer_context="unknown"
execution_context="inline"
output_contract="json_block"
case "$event" in
  PostToolUse)
    tool_name="$(printf '%s' "$input" | jq -r '.tool_name // empty' 2>/dev/null || true)"
    [[ "$tool_name" == "ReportFindings" ]] || exit 0
    # The findings key must be PRESENT and an array (assert a positive
    # marker, never an absence): a payload without the key says nothing
    # about the review outcome and must not attest. select() empties the
    # output for every non-array shape, and the shared empty-payload guard
    # below turns that into silence before any classifier call.
    findings_payload="$(printf '%s' "$input" | jq -c '
      .tool_input.findings? | select(type == "array")
    ' 2>/dev/null || true)"
    # ReportFindings is a tool call in the authoring turn. That is positive
    # same-context evidence, not a guess from who typed the command.
    reviewer_context="shared"
    execution_context="inline"
    output_contract="report_findings"
    ;;
  SubagentStop)
    message="$(printf '%s' "$input" | jq -r '.last_assistant_message // empty' 2>/dev/null || true)"
    [[ -n "$message" ]] || exit 0

    # THREE independent signals identify this as a code-review completion, any
    # one sufficient. Signals 1 and 3 are string matches on things the tool
    # does not mandate; signal 2 is the one that always holds, and it is why
    # this branch stopped being decorative (x-bcb5).
    #
    # Measured live on 2026-08-17, one `Skill(skill="code-review")` fork:
    # SubagentStop DOES fire (target-subagent-guard.sh logged its
    # `subagent_done` row as a positive control), but it fires with
    # `agent_type` = "general-purpose" and with NO agent_name / description /
    # subagent_description at all - the guard, which reads that same fallback
    # chain, recorded `agent:"unknown"`. The final text was the literal
    # "(none)". So signal 1 could not match, signal 3 could not match, and the
    # fence parser had nothing to parse. Six PRs shipped green and unmergeable
    # on that hole, each rescued by a hand-run emit-attestation.sh.
    #
    # 1. `agent_type`, the one DOCUMENTED payload field, and the only one the
    #    harness controls. Every other name in play is CALLER CHOSEN: the
    #    payload's agent_name and description fields are spawn names and task
    #    prose, and .meta.json's name is a spawn name too (live census: values
    #    like finder-A; every sidecar whose name matched the reviewer regex
    #    also carried the forked-skill marker). A caller-chosen string cannot
    #    identify a review, so none of them is read.
    #
    # The reviewer identity rule lives HERE and only here; every signal reads
    # it through this variable, so one edit cannot leave two spellings
    # disagreeing about what counts as a code review.
    REVIEWER_RE='^/?code-review([[:space:]]|$)'
    described=0
    agent_type="$(printf '%s' "$input" | jq -r '.agent_type? // empty' 2>/dev/null || true)"
    [[ "$agent_type" =~ $REVIEWER_RE ]] && described=1

    # 2. The harness's own record of WHAT THIS FORK RAN, which it writes
    #    beside the subagent transcript whatever the skill's output contract
    #    says. The marker sidecars are the skill-fork record:
    #    `agent-<id>.forked-skill.marker.json` holds
    #    {"forkedSkill":true,"skillName":"code-review"}. The forkedSkill flag
    #    is the part a plain spawned task cannot carry, so it gates the match.
    #    Nothing the review prints can dodge it, and no unrelated subagent
    #    can forge it.
    #
    #    Fail direction is unchanged: a missing or renamed sidecar leaves this
    #    signal at 0 and the gate holds, rather than attesting on a guess.
    forked=0
    agent_transcript="$(printf '%s' "$input" | jq -r '.agent_transcript_path // empty' 2>/dev/null || true)"
    agent_transcript="${agent_transcript/#\~/$HOME}"
    if [[ -n "$agent_transcript" ]]; then
      sidecar_base="${agent_transcript%.jsonl}"
      # Only the marker file carries the forkedSkill flag in live harness
      # data (checked over 200 real sidecars); the sibling .forked-skill.json
      # does not, so listing it here would advertise a fallback that never
      # fires.
      if [[ -f "$sidecar_base.forked-skill.marker.json" ]] \
        && jq -e --arg re "$REVIEWER_RE" \
          '(.forkedSkill == true) and ((.skillName? // "") | test($re))' \
          "$sidecar_base.forked-skill.marker.json" >/dev/null 2>&1; then
        forked=1
      fi
    fi

    # 3. The message's own shape: "## Review findings" OPENING the message, a
    #    shape two live self-reviews produced (x-e97b). ReportFindings
    #    mandates no header, so this was never sound on its own; it is kept
    #    because it costs nothing and still covers a harness that populates
    #    neither of the above. Opening-anchor, not line-anchor: an unrelated
    #    subagent quoting the heading mid-text does not satisfy it.
    shaped=0
    [[ "$message" =~ ^[[:space:]]*"## Review findings" ]] && shaped=1

    [[ "$described" == "1" || "$forked" == "1" || "$shaped" == "1" ]] || exit 0
    record_review_subagent_marker
    [[ "$forked" == "1" ]] && reviewer_context="fresh"
    if [[ "$forked" == "1" ]]; then
      execution_context="fork"
      output_contract="json_block"
    fi

    # The clean-pass verdict, positively enumerated over every shape observed
    # live. An empty fenced JSON array is the high-level protocol's marker; a
    # final text that is the literal "(none)" and nothing else is the
    # low-level protocol's. Both mean the same thing, and a parser that reads
    # only the first calls the second unparseable - which is a silence
    # indistinguishable from "no review ran". Reading only the LAST fence is
    # no better in the other direction: a findings fence followed by an
    # empty "excluded files" fence would read clean. The verdict is clean
    # only when EVERY fence in the message parses to an empty list.
    findings="$(printf '%s' "$message" | python3 -c '
import json, re, sys
text = sys.stdin.read()
fences = re.findall(r"```json\s*(.*?)```", text, re.DOTALL)
if not fences:
    print("absent")
    sys.exit(0)
for body in fences:
    try:
        data = json.loads(body)
    except Exception:
        print("absent")
        sys.exit(0)
    if data != []:
        print("nonempty")
        sys.exit(0)
print("[]")
' 2>/dev/null || echo "absent")"
    if [[ "$findings" == "[]" ]]; then
      findings_payload="[]"
    elif [[ "$findings" == "absent" ]] \
      && [[ "$message" =~ ^[[:space:]]*\(none\)[[:space:]]*$ ]]; then
      # Only when NO array was parsed and the ENTIRE final text is the bare
      # marker - the observed protocol spells its whole verdict that way. A
      # standalone "(none)" line inside longer output, an excuse line above
      # it especially, attests nothing.
      findings_payload="[]"
    fi
    ;;
  Stop)
    turn_id="$(printf '%s' "$input" | jq -r '.turn_id // empty' 2>/dev/null || true)"
    transcript="$(printf '%s' "$input" | jq -r '.transcript_path // empty' 2>/dev/null || true)"
    [[ -n "$turn_id" && -n "$transcript" ]] || exit 0
    transcript="${transcript/#\~/$HOME}"
    [[ -r "$transcript" ]] || exit 0

    # Codex has emitted both a direct `exited_review_mode` payload and an
    # `item_completed` payload containing an `ExitedReviewMode` item. Both are
    # structured transcript evidence, not final assistant prose. Slurp the
    # complete JSONL so a malformed later row, a duplicate completion, or a
    # wrong-turn row cannot be normalized into a clean result by a partial grep.
    review_outputs="$(jq -s --arg turn "$turn_id" '
      [
        .[]
        | select(.type == "event_msg" and .payload.turn_id == $turn)
        | if (.payload.type == "item_completed"
              and .payload.item.type == "ExitedReviewMode") then
            .payload.item.review_output
          elif (.payload.type == "exited_review_mode") then
            .payload.review_output
          else
            empty
          end
      ]
    ' "$transcript" 2>/dev/null)" || exit 0

    # Exactly one completion with a present array-valued findings key is the
    # positive review marker, empty or not: the findings go to the
    # classifier, which decides pass from fail. Absent findings key or any
    # other shape stays silent - absence is not a review.
    jq -e '
      length == 1
      and (.[0] | type == "object"
        and has("findings")
        and (.findings | type == "array"))
    ' <<<"$review_outputs" >/dev/null 2>&1 || exit 0
    findings_payload="$(jq -c '.[0]' <<<"$review_outputs")"
    # Codex's Stop payload proves the structured review result, but this hook
    # has no positive evidence that the reviewer ran in a separate context.
    reviewer_context="unknown"
    execution_context="inline"
    output_contract="json_block"
    ;;
  *)
    exit 0
    ;;
esac

[[ -n "$findings_payload" ]] || exit 0

# Emit from the SESSION cwd, not the plugin root: a worktree session pins its
# own HEAD, and the event log is per-checkout.
cwd="$(printf '%s' "$input" | jq -r '.cwd // empty' 2>/dev/null || true)"
[[ -n "$cwd" ]] || exit 0
cd "$cwd"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Classify once, decide the verdict from the blocking count, and hand the
# SAME payload to the emitter (which re-classifies it into the event record;
# the rule is pure, so the two invocations cannot disagree).
payload_file="$(mktemp -t code-review-attest-findings.XXXXXX)"
trap 'rm -f "$payload_file"' EXIT
printf '%s' "$findings_payload" > "$payload_file"
if ! record="$("${FNO:-fno}" do review classify --findings-file "$payload_file" --emit-record)" 2>/dev/null; then
  echo "code-review-attest: classify refused the findings payload; no event emitted (a deployment without 'do review classify' is too old for this hook)" >&2
  exit 0
fi
blocking="$(jq -r '.findings_blocking // 1' <<<"$record" 2>/dev/null || echo 1)"
nonblocking="$(jq -r '.findings_nonblocking // 0' <<<"$record" 2>/dev/null || echo 0)"
total="$(jq -r '(.findings | length) // 0' <<<"$record" 2>/dev/null || echo 0)"
case "$blocking" in
  ''|*[!0-9]*) blocking=1 ;;  # an unreadable count is not zero findings
esac
if [[ "$blocking" == "0" ]]; then
  verdict="pass"
else
  verdict="fail"
fi
# The one positive line naming what was classified: a run where the
# classifier never fired is visibly different from one that classified zero.
echo "code-review-attest: classified $total finding(s): $blocking blocking, $nonblocking non-blocking"

bash "$script_dir/../skills/review/scripts/emit-attestation.sh" code-review "$verdict" \
  "$reviewer_context" "$execution_context" "$output_contract" --findings-file "$payload_file"
