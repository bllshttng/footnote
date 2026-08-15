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
# Both paths converge on the SAME clean-pass signal: an empty findings
# array, matching ReportFindings' own contract ("empty array if nothing
# survived verification").
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
    # The subagent's task description names the invocation. Try every field
    # name this repo's own hooks have observed a subagent's task carried
    # under (target-subagent-guard.sh's same fallback chain), so a harness
    # version that renames the field does not silently stop matching.
    description="$(printf '%s' "$input" | jq -r '.agent_name // .description // .subagent_description // empty' 2>/dev/null || true)"
    [[ "$description" =~ ^/?code-review([[:space:]]|$) ]] || exit 0
    message="$(printf '%s' "$input" | jq -r '.last_assistant_message // empty' 2>/dev/null || true)"
    [[ -n "$message" ]] || exit 0
    findings="$(printf '%s' "$message" | python3 -c '
import json, re, sys
text = sys.stdin.read()
m = re.search(r"```json\s*(.*?)```", text, re.DOTALL)
if not m:
    print("absent")
    sys.exit(0)
try:
    data = json.loads(m.group(1))
except Exception:
    print("absent")
    sys.exit(0)
print("[]" if data == [] else "nonempty")
' 2>/dev/null || echo "absent")"
    [[ "$findings" == "[]" ]] && is_clean=1
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
