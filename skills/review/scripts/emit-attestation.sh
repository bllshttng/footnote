#!/usr/bin/env bash
# Emit a head-pinned review_attestation event (x-e703, Phase 2).
#
# This is the single producer surface for the config.review.reviewers gate: a
# local reviewer that leaves NO GitHub review object emits this event so
# `fno-agents loop-check` can read it as gate evidence. The reviewer name is
# NOT an allowlist here - a project-registered reviewer
# (config.review.reviewer_registry, x-a534) attests through this same helper,
# which is why the registry needed no new producer machinery. loop-check head-pins on the CURRENT HEAD - a pass on a prior commit
# stops counting the moment a new commit lands, so this MUST run after the
# reviewed commit is the tip.
#
# Usage: emit-attestation.sh <reviewer> [verdict]
#   <reviewer>  a built-in (sigma | peer | code-review | declare) or any name declared
#               in config.review.reviewer_registry (a leading '/' is stripped)
#   [verdict]   pass (default) | fail
set -euo pipefail

reviewer="${1:?reviewer name required: a built-in (sigma|code-review|declare) or a config.review.reviewer_registry name}"
verdict="${2:-pass}"
while [[ "$reviewer" == /* ]]; do reviewer="${reviewer#/}"; done # strip ALL leading slashes (parity with both parsers' lstrip / trim_start_matches)

head_sha="$(git rev-parse HEAD 2>/dev/null)" || {
  echo "emit-attestation: not a git repo (cannot head-pin); no event emitted" >&2
  exit 1
}

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

# Record WHICH MODEL rendered the verdict. Model routing stamps ANTHROPIC_MODEL
# (and every tier var) for the whole worker process, so a worker routed to a
# cheap secondary provider renders its own review verdict there - and no
# per-spawn role guard can see it, because the verdict is a later activity
# inside an already-routed process. Reading the env is the only signal available
# at emit; it reports what the environment CLAIMED, never proof of the model
# that answered, so both stay empty rather than defaulting to a guess a later
# reader would mistake for evidence. Empty means NOT OBSERVABLE, not "primary":
# resolve_codex_route carries a codex worker's route in `-c model=...` config
# args with only the API key in env, so a routed codex verdict reads empty here.
model="${ANTHROPIC_MODEL:-}"
provider=""
if [[ -n "${ANTHROPIC_BASE_URL:-}" ]]; then
  provider="${ANTHROPIC_BASE_URL#*://}"   # strip scheme
  provider="${provider%%/*}"              # host only, no path
  provider="${provider##*@}"              # drop userinfo: a key in the URL must not land in the log
fi

# Record the harness session of the EMITTING PROCESS. This is the only field
# that can tell an author-attested review from an independent one: session_id
# (above) is grepped from the worktree manifest, so it equals manifest.session_id
# for every emitter in this worktree - including a reviewer who is genuinely not
# the author - and a join on it returns 'self' 100% of the time by construction.
# The live process env is what makes this field vary with who emitted. Read on
# the same marker precedence as init-target-state.sh, and NEVER fall back to the
# manifest: a fallback would restore the tautology under a new name. Empty when
# no marker is set, never guessed.
attester_session_id=""
for _marker in CODEX_THREAD_ID CLAUDE_CODE_SESSION_ID CODEX_SESSION_ID GEMINI_SESSION_ID; do
  if [[ "${!_marker:-}" == *[![:space:]]* ]]; then
    attester_session_id="${!_marker}"
    break
  fi
done

# Build the data object with jq so a reviewer/verdict value can never break the
# JSON (codex peer review P2). fno event emit then validates envelope + required
# fields + the verdict enum before writing.
data="$(jq -cn --arg reviewer "$reviewer" --arg head_sha "$head_sha" --arg verdict "$verdict" \
  --arg session_id "$session_id" --arg harness "$harness" \
  --arg model "$model" --arg provider "$provider" \
  --arg attester_session_id "$attester_session_id" \
  '{reviewer:$reviewer,head_sha:$head_sha,verdict:$verdict,session_id:$session_id,harness:$harness,model:$model,provider:$provider,attester_session_id:$attester_session_id}')"
# FNO overrides the binary (defaults to the mux); tests point it at fno-py,
# which is on PATH in the uv test env where the mux is not installed.
"${FNO:-fno}" event emit -t review_attestation -s target -d "$data"

echo "review_attestation emitted: reviewer=$reviewer head_sha=${head_sha:0:8} verdict=$verdict session=${session_id:-none} attester=${attester_session_id:-none} harness=${harness:-unknown} model=${model:-unobserved} provider=${provider:-unobserved}" >&2
