#!/usr/bin/env bash
# Emit the code-review attestation when a native /code-review pass reports
# CLEAN (x-e97b). Two independent triggers, wired in hooks.json under two
# different hook events, because /code-review reaches a clean pass through
# two different reachable paths and a guard on only one is decorative
# (AGENTS.md pitfalls corpus, "a guard placed on one of N reachable paths"):
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
# Trigger 2 identifies the fork from the harness's own record of what it
# ran (the sidecar beside `agent_transcript_path`), NOT from the shape of
# what it printed. Keying on printed shape is what left this branch
# decorative through six PRs: see the measurement in the branch itself.
#
# The clean-pass signal is an empty findings set, matching ReportFindings'
# own contract ("empty array if nothing survived verification"). The two
# review protocols spell it differently - an empty fenced JSON array, or a
# final text that is the bare "(none)" marker and nothing else - and both
# are accepted, by name, nothing else.
#
# Fail direction: any parse problem, an event this script does not
# recognize, or a NON-empty findings array emits nothing, so the reviewers
# gate holds rather than clearing on evidence that never arrived. A review
# that found bugs emits nothing on purpose: the fixes move HEAD, and the
# freshness protocol would kill a premature attestation anyway. Re-run the
# review on the new HEAD; this hook fires on that pass's clean report.
set -euo pipefail

input="$(cat)"
event="$(printf '%s' "$input" | jq -r '.hook_event_name // empty' 2>/dev/null || true)"

is_clean=0
case "$event" in
  PostToolUse)
    tool_name="$(printf '%s' "$input" | jq -r '.tool_name // empty' 2>/dev/null || true)"
    [[ "$tool_name" == "ReportFindings" ]] || exit 0
    # A clean pass is the findings key PRESENT and an EMPTY array. Absent is
    # not clean (assert a positive marker, never an absence): a payload
    # without the key says nothing about the review outcome and must not
    # attest.
    findings="$(printf '%s' "$input" | jq -c '.tool_input.findings? // "absent"' 2>/dev/null || true)"
    [[ "$findings" == "[]" ]] && is_clean=1
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
    # 1. A name field names the invocation. `agent_type` is the DOCUMENTED
    #    one; the other three are names this repo's hooks have observed a
    #    subagent's task carried under. Each is tested on its own rather than
    #    first-non-null, because agent_type is always populated and a `//`
    #    chain starting with it would shadow every later field.
    # The reviewer identity rule lives HERE and only here; every signal reads
    # it through this variable, so one edit cannot leave two spellings
    # disagreeing about what counts as a code review.
    REVIEWER_RE='^/?code-review([[:space:]]|$)'
    described=0
    while IFS= read -r cand; do
      [[ "$cand" =~ $REVIEWER_RE ]] && described=1
    done < <(printf '%s' "$input" \
      | jq -r '[.agent_type?, .agent_name?, .description?, .subagent_description?]
               | map(select(type == "string")) | .[]' 2>/dev/null || true)

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
      for sidecar in "$sidecar_base.forked-skill.marker.json" \
        "$sidecar_base.forked-skill.json"; do
        [[ -f "$sidecar" ]] || continue
        if jq -e --arg re "$REVIEWER_RE" \
          '(.forkedSkill == true) and ((.skillName? // "") | test($re))' \
          "$sidecar" >/dev/null 2>&1; then
          forked=1
          break
        fi
      done
      # `.meta.json` is written for EVERY subagent, spawned tasks included,
      # and its description is caller prose: a task told to "code-review the
      # failing tests" is not a review fork. Only the harness-recorded name
      # field counts.
      if [[ "$forked" == "0" && -f "$sidecar_base.meta.json" ]] \
        && jq -e --arg re "$REVIEWER_RE" '(.name? // "") | test($re)' \
          "$sidecar_base.meta.json" >/dev/null 2>&1; then
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

    # The clean-pass verdict, positively enumerated over every shape observed
    # live. An empty fenced JSON array is the high-level protocol's marker; a
    # final text that is the literal "(none)" and nothing else is the
    # low-level protocol's. Both mean the same thing, and a parser that reads
    # only the first calls the second unparseable - which is a silence
    # indistinguishable from "no review ran".
    findings="$(printf '%s' "$message" | python3 -c '
import json, re, sys
text = sys.stdin.read()
fences = re.findall(r"```json\s*(.*?)```", text, re.DOTALL)
if not fences:
    print("absent")
    sys.exit(0)
try:
    data = json.loads(fences[-1])
except Exception:
    print("absent")
    sys.exit(0)
print("[]" if data == [] else "nonempty")
' 2>/dev/null || echo "absent")"
    if [[ "$findings" == "[]" ]]; then
      is_clean=1
    elif [[ "$findings" == "absent" ]] \
      && [[ "$message" =~ ^[[:space:]]*\(none\)[[:space:]]*$ ]]; then
      # Only when NO array was parsed and the ENTIRE final text is the bare
      # marker - the observed protocol spells its whole verdict that way. A
      # standalone "(none)" line inside longer output, an excuse line above
      # it especially, attests nothing.
      is_clean=1
    fi
    ;;
  *)
    exit 0
    ;;
esac

[[ "$is_clean" == "1" ]] || exit 0

# Emit from the SESSION cwd, not the plugin root: a worktree session pins its
# own HEAD, and the event log is per-checkout.
cwd="$(printf '%s' "$input" | jq -r '.cwd // empty' 2>/dev/null || true)"
[[ -n "$cwd" ]] || exit 0
cd "$cwd"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bash "$script_dir/../skills/review/scripts/emit-attestation.sh" code-review
