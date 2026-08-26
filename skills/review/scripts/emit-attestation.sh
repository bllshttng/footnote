#!/usr/bin/env bash
# Emit a head-pinned review_attestation event (x-e703, Phase 2).
#
# WHAT THIS CERTIFIES: a CLEAN review at the current head. It is a hand-run of
# a producer that missed, never a way past an open finding. Running it over
# unresolved findings makes the gate the whole board trusts tell a lie, and
# nothing downstream can tell that apart from a real pass.
#
# This is the single producer surface for the config.review.reviewers gate: a
# local reviewer that leaves NO GitHub review object emits this event so
# `fno-agents loop-check` can read it as gate evidence. The reviewer name is
# NOT an allowlist here - a project-registered reviewer
# (config.review.reviewer_registry, x-a534) attests through this same helper,
# which is why the registry needed no new producer machinery.
#
# STILL RUN THIS AFTER THE REVIEWED COMMIT IS THE TIP. The old rule here was
# "a pass on a prior commit stops counting the moment a new commit lands",
# and that is no longer true (x-5b99 / x-62a1): loop-check now decides
# freshness from the PR's own code-diff identity, so an attestation carries
# across a rebase or a documentation-only advance. It still dies on any real
# code change, and it still dies on every failure path, so emitting before
# committing pins a commit your findings are not about and buys nothing.
#
# WHAT THIS EVENT PROVES: that a commit was pinned. NOT that a review
# happened. The review verb emits nothing itself, and this script records the
# reviewer name and verdict it is PASSED. Nothing here can tell a real review
# from a caller that typed the arguments.
#
# Usage: emit-attestation.sh <reviewer> [verdict] [reviewer_context] [execution_context] [output_contract] [--findings-file <path>]
#   <reviewer>  a built-in (sigma | peer | code-review | declare) or any name declared
#               in config.review.reviewer_registry (a leading '/' is stripped)
#   [verdict]   pass (default) | fail
#   [reviewer_context]  fresh | shared | unknown (default unknown); positive
#                       context evidence only, never inferred from the sender
#   [execution_context] inline (default) | fork; where the review ran
#   [output_contract]   json_block (default) | report_findings; the contract
#                       the review's result surfaced under
#   [--findings-file <path>]  a JSON findings payload; classified by `fno do
#               review classify` and carried on the event as the finding record.
#               A malformed or unreadable file is a refusal, never an empty record.
set -euo pipefail

usage() {
  # Anchored on the `# Usage:` marker, never on line numbers. A `sed -n
  # '13,16p'` prints whatever happens to sit at those lines, so it silently
  # printed the wrong paragraph the moment the header above it grew - which is
  # exactly what happened here, and the only reason it was caught is that a
  # test asserted the literal "Usage:". Print from the marker to the end of
  # that comment block instead, so the block can move freely.
  sed -n '/^# Usage:/,/^[^#]/p' "$0" | sed '/^[^#]/d; s/^# \{0,1\}//'
}

# A flag-shaped first argument is NEVER a reviewer name. Without this, `--help`
# was accepted as one and emitted a real `verdict=pass` attestation against the
# current HEAD: asking this script how to run it wrote evidence onto the merge
# gate. Any typo'd or stray flag did the same. There is no reviewer name in any
# registry that begins with `-`, so refusing the whole shape costs nothing and
# closes the class rather than the one spelling.
case "${1:-}" in
  -h | --help)
    usage
    exit 0
    ;;
  -*)
    echo "emit-attestation: '$1' is a flag, not a reviewer name; no event emitted" >&2
    echo >&2
    usage >&2
    exit 2
    ;;
esac

# --findings-file <path> (positionals first, flag after): the classified
# finding record rides onto the event. The classification itself is `fno do
# review classify --emit-record`, the one shell entry point, so the blocking
# rule is never reimplemented in bash. A malformed or unreadable findings
# file is a refusal, not an empty record: an unmeasurable finding set must
# not attest as though it classified zero.
findings_file=""
prev_arg=""
positional=()
for arg in "$@"; do
  if [[ "$prev_arg" == "--findings-file" ]]; then
    findings_file="$arg"
    prev_arg=""
    continue
  fi
  if [[ "$arg" == "--findings-file" ]]; then
    prev_arg="$arg"
    continue
  fi
  positional+=("$arg")
done
[[ -z "$prev_arg" ]] || {
  echo "emit-attestation: --findings-file needs a path; no event emitted" >&2
  exit 2
}

reviewer="${positional[0]:?reviewer name required: a built-in (sigma|code-review|declare) or a config.review.reviewer_registry name}"
verdict="${positional[1]:-pass}"
reviewer_context="${positional[2]:-unknown}"
execution_context="${positional[3]:-inline}"
output_contract="${positional[4]:-json_block}"
case "$reviewer_context" in
  fresh|shared|unknown) ;;
  *)
    echo "emit-attestation: reviewer_context must be fresh, shared, or unknown (got '$reviewer_context')" >&2
    exit 2
    ;;
esac
case "$execution_context" in
  inline|fork) ;;
  *) echo "emit-attestation: execution_context must be inline or fork (got '$execution_context')" >&2; exit 2 ;;
esac
case "$output_contract" in
  report_findings|json_block) ;;
  *) echo "emit-attestation: output_contract must be report_findings or json_block (got '$output_contract')" >&2; exit 2 ;;
esac
while [[ "$reviewer" == /* ]]; do reviewer="${reviewer#/}"; done # strip ALL leading slashes (parity with both parsers' lstrip / trim_start_matches)

head_sha="$(git rev-parse HEAD 2>/dev/null)" || {
  echo "emit-attestation: not a git repo (cannot head-pin); no event emitted" >&2
  exit 1
}

# The journal is SHARED across worktrees by design (setup-worktree.sh links
# every worktree's .fno/events.jsonl to canonical), so head_sha alone cannot
# say which PR an attestation is about: two branches can carry the same code
# delta and a global head-sha match then counts a foreign review. The branch
# is what scopes the event to the PR that was reviewed.
#
# Record the branch's UPSTREAM short name only when it names a PR branch: a
# spawned reviewer runs in its own worktree on a branch of its own (git
# refuses two worktrees on one branch), and a reviewer worktree created from
# the PR branch tracks it - so the upstream names the branch GitHub reports
# as headRefName and the LOCAL name never would. But an fno AUTHOR worktree
# is created off the repo's default branch and tracks IT until `push -u`
# fires at PR create, so an upstream naming the BASE, not the PR, would
# mis-scope every pre-push emit and kill the branch-arm carry this field
# exists to preserve. The base is whatever `refs/remotes/origin/HEAD`
# actually points at (round 3, PR 917: a literal `main` comparison recorded
# `branch=develop` on every develop-based repo, scoping the author's
# feature-branch attestation to a branch no PR would ever carry), falling
# back to `main` only when the symbolic ref is unset - a fresh clone without
# it behaves exactly as before. A detached HEAD names no branch at all:
# refuse rather than record "" - the empty string is byte-identical to the
# pre-branch-field backlog, so a live emit would mint a fresh legacy member
# no later carry can scope. git rev-parse is local and free; do NOT reach
# for `gh pr view` here - a network call on the emit path turns a review
# receipt into something that fails when GitHub is slow.
branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
if [[ -z "$branch" || "$branch" == "HEAD" ]]; then
  echo "emit-attestation: detached HEAD names no PR branch (an empty branch field would read as a pre-branch-field event and never carry past a head move); no event emitted" >&2
  exit 1
fi
base="$(git symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null || true)"
base="${base#*/}"
base="${base:-main}"
upstream="$(git rev-parse --abbrev-ref --symbolic-full-name '@{upstream}' 2>/dev/null || true)"
# The upstream is the PR branch only when this checkout carries NO WORK OF ITS
# OWN. That is what separates the two shapes, and the base name never was: an
# author worktree tracking any branch other than origin/HEAD (origin/HEAD at
# main, the worktree created off origin/develop) passes the name test and
# recorded branch=develop - the round-3 `main`-literal bug relocated, since it
# still loses the branch arm for the real PR AND puts the event in scope for
# any PR whose headRefName is literally develop. A reviewer worktree sits AT
# the PR branch tip it tracks, so `@{upstream}..HEAD` is empty; an author
# worktree has commits ahead of whatever it tracks, which is precisely the
# diff under review. Count the commits, do not guess from names.
#
# The name test stays as the second conjunct, and it is not redundant: a
# just-created author worktree has zero commits yet, and without it that
# worktree would record its BASE as the branch. Both must hold.
#
# Deliberately NOT `gh pr view --json baseRefName`. It costs a network call on
# the emit path, which turns a review receipt into something that fails when
# GitHub is slow, and it does not even close this bug: with a PR based on main
# and a worktree tracking origin/develop, baseRefName is main, develop != main,
# and the wrong name is recorded exactly as before.
ahead=1
if [[ -n "$upstream" ]]; then
  ahead="$(git rev-list --count '@{upstream}..HEAD' 2>/dev/null || echo 1)"
fi
if [[ "$upstream" == */* && "${upstream#*/}" != "$base" && "$ahead" == "0" ]]; then
  branch="${upstream#*/}"
fi

# The diff under review, recorded with the event: its merge-base, its head, and
# the added+deleted line count across it. Without these a clean review and a
# review of NOTHING are byte-identical downstream: a session resolving its
# target from a checkout sitting on the base branch reads a zero-line diff,
# reports clean, and mints a pass no reader can distinguish from a real one.
# The base is the resolved base BRANCH above, not the upstream - a reviewer
# worktree tracks the PR branch itself, and diffing against that would read
# zero lines for every legitimate reviewer at the PR tip.
reviewed_base_sha="$(git merge-base HEAD "origin/${base}" 2>/dev/null || git merge-base HEAD "${base}" 2>/dev/null || true)"
if [[ -z "$reviewed_base_sha" ]]; then
  echo "emit-attestation: cannot resolve the base branch '${base}' to a merge-base; the diff under review is unmeasurable, so no event emitted" >&2
  exit 1
fi
reviewed_head_sha="$head_sha"
# Files, not lines, decide whether there is anything to read: numstat prints
# "-" for binary files and 0 for pure renames and empty files, so a LINES-only
# count reads a real binary-only diff as empty and refuses the one review that
# exists. The line count stays in the record (0 is an honest measurement of a
# binary diff); the refusal fires only when the diff changed no file at all.
reviewed_file_count="$(git diff --name-only "${reviewed_base_sha}..HEAD" 2>/dev/null | command grep -c . || true)"
reviewed_line_count="$(git diff --numstat "${reviewed_base_sha}..HEAD" 2>/dev/null | awk '{ add += $1; del += $2 } END { print add + del + 0 }')"
if (( reviewed_file_count == 0 )); then
  echo "emit-attestation: the diff under review is empty (no changed files, base ${reviewed_base_sha} .. HEAD ${reviewed_head_sha} on branch ${branch})." >&2
  echo "A review with nothing to read is not a pass; no event emitted." >&2
  echo "If you are reviewing a worktree from the canonical checkout, hand the review its" >&2
  echo "target explicitly: run from the worktree path, or pass the PR number to the review verb." >&2
  exit 1
fi

# Record the attesting ACTOR alongside what was certified (x-27c5): without a
# session, an author attesting its own diff is indistinguishable from an
# independent reviewer, which clears config.review.reviewers with no trace.
# session_id + head_sha is the authorship join. Read from the live session
# manifest with the same grep the stop hook uses
# (hooks/target-stop-hook.sh). Both stay empty when no manifest is bound; the
# emit chokepoint then rejects an actorless attestation rather than lie.
repo_root="$(git rev-parse --show-toplevel 2>/dev/null || echo "$PWD")"
session_id=""; harness=""
if [[ -f "$repo_root/.fno/target-state.md" ]]; then
  session_id=$(grep '^session_id:' "$repo_root/.fno/target-state.md" \
    | head -1 | sed 's/^session_id:[[:space:]]*//' | tr -d '[:space:]' || true)
  harness=$(grep '^harness:' "$repo_root/.fno/target-state.md" \
    | head -1 | sed 's/^harness:[[:space:]]*//' | tr -d '[:space:]' || true)
fi

# The review hook owns the attempt, while this script owns the completion
# marker. Read the hold before releasing it so both records use one join id.
local_branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
hold_json=""
if [[ -n "$local_branch" ]]; then
  hold_json="$("${FNO:-fno}" do pr review-hold metadata --branch "$local_branch" \
    --repo "$repo_root" 2>/dev/null || true)"
fi
invocation_id="$(printf '%s' "$hold_json" | jq -r '.metadata.invocation_id // empty' 2>/dev/null || true)"
if [[ -z "$invocation_id" && -n "$session_id" ]]; then
  invocation_id="$(jq -r '.invocation_id // empty' \
    "${FNO_HOME:-$HOME/.fno}/review-invocations/${session_id}.json" \
    2>/dev/null || true)"
fi
if [[ -z "$invocation_id" ]]; then
  invocation_id="ri-$(date -u +%s 2>/dev/null || echo 0)-$$"
fi
review_verb="$(printf '%s' "$hold_json" | jq -r '.metadata.verb // "/code-review"' 2>/dev/null || echo /code-review)"
review_args_raw="$(printf '%s' "$hold_json" | jq -r '.metadata.args_raw // empty' 2>/dev/null || true)"
review_level="$(printf '%s' "$hold_json" | jq -r '.metadata.level // "unset"' 2>/dev/null || echo unset)"
review_level_source="$(printf '%s' "$hold_json" | jq -r '.metadata.level_source // "fallback"' 2>/dev/null || echo fallback)"
review_flags="$(printf '%s' "$hold_json" | jq -c '.metadata.flags // []' 2>/dev/null || echo '[]')"
[[ -n "$review_verb" ]] || review_verb="/code-review"
[[ -n "$review_level" ]] || review_level="unset"
[[ -n "$review_level_source" ]] || review_level_source="fallback"
[[ -n "$review_flags" ]] || review_flags='[]'
if ! jq -e 'type == "array" and all(.[]; type == "string")' <<<"$review_flags" >/dev/null 2>&1; then
  review_flags='[]'
fi
review_model_family=""
if [[ -n "$session_id" ]]; then
  review_model_family="$(jq -r '.model_family // .model // empty' \
    "${FNO_HOME:-$HOME/.fno}/attest/${session_id}.json" 2>/dev/null || true)"
fi
if [[ "$review_model_family" == "unobserved" ]]; then
  review_model_family=""
fi
count_dir="${FNO_HOME:-$HOME/.fno}/review-invocations/${invocation_id}.subagents"
subagent_count=0
if [[ -d "$count_dir" ]]; then
  for marker in "$count_dir"/*; do
    [[ -f "$marker" ]] || continue
    subagent_count=$((subagent_count + 1))
  done
fi
[[ "$subagent_count" =~ ^[0-9]+$ ]] || subagent_count=0
review_event_data="$(jq -cn \
  --arg invocation_id "$invocation_id" \
  --arg stage started \
  --arg verb "$review_verb" \
  --arg args_raw "$review_args_raw" \
  --arg level "$review_level" \
  --arg level_source "$review_level_source" \
  --argjson flags "$review_flags" \
  --arg transport skill_tool \
  --arg initiator self \
  --arg target_session_id "$session_id" \
  --arg head_sha "$head_sha" \
  --arg branch "$branch" \
  --arg model_family "$review_model_family" \
  --arg execution_context "$execution_context" \
  --arg output_contract "$output_contract" \
  --argjson subagent_count "$subagent_count" \
  '{invocation_id:$invocation_id,stage:$stage,verb:$verb,args_raw:$args_raw,level:$level,level_source:$level_source,flags:$flags,transport:$transport,initiator:$initiator,target_session_id:$target_session_id,head_sha:$head_sha,branch:$branch,execution_context:$execution_context,output_contract:$output_contract,subagent_count:$subagent_count} | if $model_family == "" then . else .model_family=$model_family end' \
  2>/dev/null || true)"
if [[ -n "$review_event_data" ]]; then
  "${FNO:-fno}" doctor event emit -t review_invocation -s daemon -d "$review_event_data" \
    --events "$repo_root/.fno/events.jsonl" >/dev/null 2>&1 || true
fi

# Record WHICH MODEL rendered the verdict. Model routing stamps ANTHROPIC_MODEL
# (and every tier var) for the whole worker process, so a worker routed to a
# cheap secondary provider renders its own review verdict there - and no
# per-spawn role guard can see it, because the verdict is a later activity
# inside an already-routed process. Reading the env is the only signal available
# at emit; it reports what the environment CLAIMED, never proof of the model
# that answered, so both stay empty rather than defaulting to a guess a later
# reader would mistake for evidence. Empty means NO CLAIM WAS MADE, not
# "primary": resolve_codex_route carries a codex worker's route in `-c model=...`
# config args with only the API key in env, so a routed codex verdict reads empty
# here. A claim that WAS made and then refused reads `unobserved` instead, so the
# two never collapse into one value - see the drift block below. The receipt line
# at the end prints the stored value verbatim for the same reason.
model="${ANTHROPIC_MODEL:-}"
provider=""
if [[ -n "${ANTHROPIC_BASE_URL:-}" ]]; then
  provider="${ANTHROPIC_BASE_URL#*://}"   # strip scheme
  provider="${provider%%/*}"              # host only, no path
  provider="${provider##*@}"              # drop userinfo: a key in the URL must not land in the log
fi

# A foreign model name over an Anthropic base is the routing-drift case: the
# request falls back to the primary Anthropic model, so ANTHROPIC_MODEL names a
# model that did NOT answer. hooks/attest-model.sh already warns at SessionStart.
# Warning there and stamping the value here anyway would put a known-false claim
# in the one record the reviewers gate reads.
#
# It records the literal `unobserved` rather than blanking the field. Empty here
# means the env carried no claim at all, so reusing it would leave one value with
# two explanations - nothing was set, versus something was set and refused - and
# a reader could not tell a declined claim from an unset field. That is the same
# assert-a-positive-marker trap this script exists to keep out of the record, one
# level down. The refusal is a finding, so it is written as one.
#
# Host match is exact-or-subdomain so notanthropic.com stays foreign. The RULE
# is hooks/attest-model.sh's and this copy may not extend it: a skill script
# cannot source outside its own directory, so the rule lives in two bodies held
# equal by tests/hooks/test_attest_model.sh, which drives both over one env
# matrix (aliases, case, padding, userinfo hosts included) and fails when they
# disagree. Change the rule there first, then mirror it here, or the matrix
# goes red.
drift_host="${ANTHROPIC_BASE_URL:-}"
drift_host="${drift_host#*://}"; drift_host="${drift_host%%/*}"
drift_host="${drift_host##*@}"; drift_host="${drift_host%%:*}"
drift_host="$(printf '%s' "$drift_host" | tr '[:upper:]' '[:lower:]')"
# Judge a trimmed, case-folded copy like the hook does (Python's
# is_anthropic_model lowercases, and "Claude-Haiku-4-5" is coherent); store the
# original verbatim when the claim stands.
claim="${model#"${model%%[![:space:]]*}"}"
claim="${claim%"${claim##*[![:space:]]}"}"
claim="$(printf '%s' "$claim" | tr '[:upper:]' '[:lower:]')"
case "$claim" in
  ""|claude-*|opus|sonnet|haiku|fable) ;;
  *)
    if [[ -z "$drift_host" || "$drift_host" == "anthropic.com" || "$drift_host" == *.anthropic.com ]]; then
      model="unobserved"
    fi
    ;;
esac

# The harness session of the EMITTING PROCESS (attester_session_id) is NOT read
# here. A script-side read of its own env is one `VAR=<other-session>` assignment
# away from writing the attestation under any session id, refreshing that
# session's stale verdict onto a head it never saw. `fno doctor event emit`
# stamps the attester itself from resolve_attester_identity(), which corroborates
# the env value against the process ancestry and refuses the override shape, and
# it stamps attester_witness (process | env_only) saying whether the ancestry
# corroborated. This script therefore passes no attester of its own; a supplied
# one that disagrees with the resolved identity is refused by the emit
# chokepoint, never silently dropped.

# Build the data object with jq so a reviewer/verdict value can never break the
# JSON (codex peer review P2). fno doctor event emit then validates envelope + required
# fields + the verdict enum before writing.
# A findings file, when given, contributes its record through the SAME jq
# build: only the six event-schema keys project across, nulls dropped, so a
# classify output can never smuggle an unregistered field onto a trust-core
# event and the no-file path stays byte-identical to today.
findings_json="null"
if [[ -n "$findings_file" ]]; then
  if [[ ! -f "$findings_file" ]]; then
    echo "emit-attestation: findings file not found: $findings_file; no event emitted" >&2
    exit 2
  fi
  if ! classify_out="$("${FNO:-fno}" do review classify --findings-file "$findings_file" --emit-record)"; then
    echo "emit-attestation: classify refused the findings file $findings_file; no event emitted" >&2
    exit 2
  fi
  if ! findings_json="$(jq -c '
    {findings_blocking, findings_nonblocking, findings, findings_truncated, review_round, dispositions}
    | with_entries(select(.value != null))
  ' <<<"$classify_out" 2>/dev/null)"; then
    echo "emit-attestation: classify produced no parsable record for $findings_file; no event emitted" >&2
    exit 2
  fi
fi
data="$(jq -cn --arg reviewer "$reviewer" --arg head_sha "$head_sha" --arg verdict "$verdict" \
  --arg session_id "$session_id" --arg harness "$harness" \
  --arg model "$model" --arg provider "$provider" \
  --arg reviewer_context "$reviewer_context" \
  --arg invocation_id "$invocation_id" \
  --arg branch "$branch" \
  --arg reviewed_base_sha "$reviewed_base_sha" \
  --arg reviewed_head_sha "$reviewed_head_sha" \
  --argjson reviewed_line_count "$reviewed_line_count" \
  --argjson reviewed_file_count "$reviewed_file_count" \
  --argjson findings "$findings_json" \
  '{reviewer:$reviewer,head_sha:$head_sha,verdict:$verdict,session_id:$session_id,harness:$harness,model:$model,provider:$provider,reviewer_context:$reviewer_context,invocation_id:$invocation_id,branch:$branch,reviewed_base_sha:$reviewed_base_sha,reviewed_head_sha:$reviewed_head_sha,reviewed_line_count:$reviewed_line_count,reviewed_file_count:$reviewed_file_count} + $findings')"
# FNO overrides the binary (defaults to the mux); tests point it at fno-py,
# which is on PATH in the uv test env where the mux is not installed.
"${FNO:-fno}" doctor event emit -t review_attestation -s target -d "$data"

# The review has landed a verdict for this head, so the hold that said one was
# RUNNING has nothing left to protect. Released HERE, at the positive completion
# marker, so the release and the proof of completion are one event and cannot
# drift: a release wired to a separate "the tool returned" signal fires while
# the review is still writing fixes, which is the whole defect.
#
# NO --holder. The hold is a lane lock, not an ownership assertion, and this
# script cannot reconstruct the acquiring holder: the hook names the HARNESS
# session, while `session_id` here is grepped out of target-state.md and falls
# back to "unknown". `release_claim` no-ops SILENTLY on a mismatch, so passing
# one wedged the branch's merge lane for the full TTL under a receipt that said
# "released".
#
# Both branch spellings, because they can differ. `branch` above is rewritten to
# the upstream-derived name for a reviewer worktree, while the hook keys on the
# local `rev-parse --abbrev-ref HEAD`. Releasing one name leaves the other held.
#
# Best-effort. A release failure leaves a hold that ages out on its TTL with a
# receipt, and an attestation must never fail because a lockfile survived.
for _b in "${branch:-}" "$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"; do
  [[ -n "$_b" && "$_b" != "HEAD" ]] || continue
  # Releasing the same name twice costs one no-op and says "no hold" the second
  # time, which is cheaper than a dedup nobody reads.
  "${FNO:-fno}" do pr review-hold release --branch "$_b" >/dev/null 2>&1 || true
done

echo "review_attestation emitted: reviewer=$reviewer head_sha=${head_sha:0:8} branch=${branch:-detached} verdict=$verdict session=${session_id:-none} harness=${harness:-unknown} model=${model:-unset} provider=${provider:-unset} reviewer_context=$reviewer_context lines=$reviewed_line_count files=$reviewed_file_count" >&2
