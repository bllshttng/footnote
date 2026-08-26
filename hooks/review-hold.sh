#!/usr/bin/env bash
# Register (and clear) the hold that says a review of this branch is RUNNING.
#
# Merge readiness models a review as a RECORDED VERDICT: `review_coverage`
# answers what verdicts EXIST for a head, and a review still writing its fixes
# has produced none yet. So a PR reads green, settled and `ready: true` while a
# review of that exact head is mid-flight - three PRs on 2026-08-22, one of
# them with five counted findings under repair. A merge taken in that window
# ships the pre-review code and discards the fixes.
#
# THIS is the registration site that matters. All three specimens were reviews
# the worker self-invoked through the Skill tool, which is not footnote code and
# therefore cannot register a hold on its own. The hook can.
#
# Contract: NEVER blocks. This runs on PreToolUse, and a review that refuses to
# start because a lockfile write failed is strictly worse than a review that
# runs unheld - the worktree layer of the guard covers exactly that case. Every
# failure path here exits 0 with no permission decision.
#
# ACQUIRE ONLY, deliberately. A PostToolUse release was wired here and removed:
# for an INLINE skill the Skill tool returns the SKILL.md body and the review
# runs AFTERWARDS, so the release fired within milliseconds and the hold covered
# nothing. That is precisely the dispatched-but-not-yet-edited window layer 2
# cannot see, and the window PR 1072 was merge-ready in - the guard would have
# been decorative for its own specimen.
#
# The release therefore lives at the two markers that mean the review is REALLY
# done: `skills/review/scripts/emit-attestation.sh` (a verdict now exists for
# this head) and the TTL (the reviewer died). A review that found findings holds
# the lane until a clean re-review attests, which is the intended behavior: the
# findings are unfixed.
#
# Usage: review-hold.sh acquire   (hook JSON on stdin)
set -uo pipefail

action="${1:-}"
[[ "$action" == "acquire" ]] || exit 0
# FNO overrides the binary, matching emit-attestation.sh: tests point it at a
# stub so an assertion reads a file the test owns.
FNO_BIN="${FNO:-fno}"
command -v jq >/dev/null 2>&1 || exit 0
command -v "$FNO_BIN" >/dev/null 2>&1 || exit 0

input="$(cat 2>/dev/null || true)"
[[ -n "$input" ]] || exit 0

tool="$(printf '%s' "$input" | jq -r '.tool_name // empty' 2>/dev/null || true)"
[[ "$tool" == "Skill" ]] || exit 0

# The skill name arrives under different keys across harness versions; read the
# ones that exist and normalize away a leading slash and a plugin prefix, so
# `/fno:review`, `fno:review` and `review` are one name.
skill_raw="$(printf '%s' "$input" \
  | jq -r '.tool_input.skill // .tool_input.name // .tool_input.command // empty' \
  2>/dev/null || true)"
[[ -n "$skill_raw" ]] || exit 0
skill="${skill_raw#/}"
skill="${skill##*:}"
skill="${skill%% *}"

# The harness-native review verbs (claude, codex, opencode) plus footnote's own
# review skill. Named explicitly rather than pattern-matched: a substring rule
# on "review" would fire on `code-review-attest`, `pr-review-fixes` and any
# future skill that merely mentions one, and a hold nobody meant to take is a
# merge nobody can complete.
case "$skill" in
  code-review|review|review-changes|sigma-review) ;;
  *) exit 0 ;;
esac

cwd="$(printf '%s' "$input" | jq -r '.cwd // empty' 2>/dev/null || true)"
[[ -n "$cwd" && -d "$cwd" ]] || cwd="$PWD"

branch="$(git -C "$cwd" rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
[[ -n "$branch" && "$branch" != "HEAD" ]] || exit 0

# A review of the protected branch is not a PR review, and a hold there would
# key on a branch no PR ever merges.
case "$branch" in
  main|master|develop|dev) exit 0 ;;
esac

# The session is the holder: a second review verb fired inside the same session
# on the same branch re-takes its OWN hold rather than colliding with a
# stranger's, and the release below clears the one it took.
session="$(printf '%s' "$input" | jq -r '.session_id // empty' 2>/dev/null || true)"
holder="review-session:${session:-unknown}"

head="$(git -C "$cwd" rev-parse HEAD 2>/dev/null || true)"
# The helper is best-effort because this hook must never block the review. The
# shell fallback still creates a join id when an installed fno package cannot
# be imported by the hook process.
plugin_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." 2>/dev/null && pwd || true)"
helper_path="${plugin_root:+$plugin_root/cli/src}"
invocation_id="$(PYTHONPATH="$helper_path${PYTHONPATH:+:$PYTHONPATH}" \
  python3 -c 'from fno.review.invocation import mint_invocation_id, write_pending_invocation; import sys; i=mint_invocation_id(); write_pending_invocation(target_session_id=sys.argv[1], invocation_id=i) if sys.argv[1] else None; print(i)' \
  "$session" 2>/dev/null || true)"
if [[ -z "$invocation_id" ]]; then
  invocation_id="ri-$(date -u +%s 2>/dev/null || echo 0)-$$"
fi

parsed="$(PYTHONPATH="$helper_path${PYTHONPATH:+:$PYTHONPATH}" \
  python3 -c 'import json, sys; from fno.review.invocation import parse_review_invocation; print(json.dumps(parse_review_invocation(sys.argv[1]) or {}))' \
  "$skill_raw" 2>/dev/null || true)"
args_raw="$(printf '%s' "$parsed" | jq -r '.args_raw // empty' 2>/dev/null || true)"
level="$(printf '%s' "$parsed" | jq -r '.level // "unset"' 2>/dev/null || echo unset)"
level_source="$(printf '%s' "$parsed" | jq -r '.level_source // "fallback"' 2>/dev/null || echo fallback)"
flags="$(printf '%s' "$parsed" | jq -c '.flags // []' 2>/dev/null || echo '[]')"
model_sidecar="${FNO_HOME:-$HOME/.fno}/attest/${session}.json"
model_family="$(jq -r '.model_family // .model // empty' "$model_sidecar" 2>/dev/null || true)"
data="$(jq -cn \
  --arg invocation_id "$invocation_id" \
  --arg stage started \
  --arg verb "/$skill" \
  --arg args_raw "$args_raw" \
  --arg level "$level" \
  --arg level_source "$level_source" \
  --argjson flags "$flags" \
  --arg transport skill_tool \
  --arg initiator self \
  --arg target_session_id "$session" \
  --arg head_sha "$head" \
  --arg branch "$branch" \
  --arg model_family "$model_family" \
  '{invocation_id:$invocation_id,stage:$stage,verb:$verb,args_raw:$args_raw,level:$level,level_source:$level_source,flags:$flags,transport:$transport,initiator:$initiator,target_session_id:$target_session_id,head_sha:$head_sha,branch:$branch} | if $model_family == "" then . else .model_family=$model_family end' \
  2>/dev/null || true)"
if [[ -n "$data" ]]; then
  "$FNO_BIN" doctor event emit -t review_invocation -s hook -d "$data" \
    --events "${FNO_EVENTS_PATH:-$cwd/.fno/events.jsonl}" >/dev/null 2>&1 || true
fi

"$FNO_BIN" do pr review-hold acquire \
  --branch "$branch" --head "$head" --holder "$holder" --verb "/$skill" \
  --invocation-id "$invocation_id" \
  --args-raw "$args_raw" --level "$level" --level-source "$level_source" \
  --flags-json "$flags" \
  --repo "$cwd" >/dev/null 2>&1 || true
exit 0
