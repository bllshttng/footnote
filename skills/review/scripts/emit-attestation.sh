#!/usr/bin/env bash
# Emit a head-pinned review_attestation event (x-e703, Phase 2).
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
# Usage: emit-attestation.sh <reviewer> [verdict]
#   <reviewer>  a built-in (sigma | peer | code-review | declare) or any name declared
#               in config.review.reviewer_registry (a leading '/' is stripped)
#   [verdict]   pass (default) | fail
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

reviewer="${1:?reviewer name required: a built-in (sigma|code-review|declare) or a config.review.reviewer_registry name}"
verdict="${2:-pass}"
while [[ "$reviewer" == /* ]]; do reviewer="${reviewer#/}"; done # strip ALL leading slashes (parity with both parsers' lstrip / trim_start_matches)

head_sha="$(git rev-parse HEAD 2>/dev/null)" || {
  echo "emit-attestation: not a git repo (cannot head-pin); no event emitted" >&2
  exit 1
}

# The journal is SHARED across worktrees by design (setup-worktree.sh links
# every worktree's .fno/events.jsonl to canonical), so head_sha alone cannot
# say which PR an attestation is about: two branches can carry the same code
# delta and a global head-sha match then counts a foreign review. The branch
# is what scopes the event to the PR that was reviewed. A detached HEAD prints
# the literal "HEAD", which names no branch, so it is recorded as empty rather
# than as a branch called HEAD. git rev-parse is local and free; do NOT reach
# for `gh pr view` here - a network call on the emit path turns a review
# receipt into something that fails when GitHub is slow.
branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
if [[ "$branch" == "HEAD" ]]; then branch=""; fi

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
# Host match is exact-or-subdomain so notanthropic.com stays foreign. This
# predicate is duplicated from the hook because a skill script may not source
# outside its own directory; tests/hooks/test_attest_model.sh drives both over
# one env matrix and fails when they disagree.
drift_host="${ANTHROPIC_BASE_URL:-}"
drift_host="${drift_host#*://}"; drift_host="${drift_host%%/*}"
drift_host="${drift_host##*@}"; drift_host="${drift_host%%:*}"
case "$model" in
  ""|claude*) ;;
  *)
    if [[ -z "$drift_host" || "$drift_host" == "anthropic.com" || "$drift_host" == *.anthropic.com ]]; then
      model="unobserved"
    fi
    ;;
esac

# Record the harness session of the EMITTING PROCESS. This is the only field
# that can tell an author-attested review from an independent one: session_id
# (above) is grepped from the worktree manifest, so it equals manifest.session_id
# for every emitter in this worktree - including a reviewer who is genuinely not
# the author - and a join on it returns 'self' 100% of the time by construction.
# The live process env is what makes this field vary with who emitted.
#
# Read on the HARNESS_SESSION_MARKERS precedence (cli/src/fno/harness_identity.py)
# - the same list the owned-identity resolver stamps as manifest.harness_session_id
# on the common path - so attester and author stay in one namespace. NEVER fall
# back to the manifest: a fallback would restore the tautology under a new name.
# Empty when no marker is set, never guessed. Narrow caveat: init's bash fail-open
# prefers TARGET_TRANSCRIPT_ID for a claude driver session, which this loop does
# not mirror; on a stale fno where owned-identity did not run, such a session's
# self-attestation can read as other_session rather than self_attested.
attester_session_id=""
for _marker in CODEX_THREAD_ID CLAUDE_CODE_SESSION_ID CODEX_SESSION_ID GEMINI_SESSION_ID OPENCODE_SESSION_ID; do
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
  --arg branch "$branch" \
  '{reviewer:$reviewer,head_sha:$head_sha,verdict:$verdict,session_id:$session_id,harness:$harness,model:$model,provider:$provider,attester_session_id:$attester_session_id,branch:$branch}')"
# FNO overrides the binary (defaults to the mux); tests point it at fno-py,
# which is on PATH in the uv test env where the mux is not installed.
"${FNO:-fno}" event emit -t review_attestation -s target -d "$data"

echo "review_attestation emitted: reviewer=$reviewer head_sha=${head_sha:0:8} branch=${branch:-detached} verdict=$verdict session=${session_id:-none} attester=${attester_session_id:-none} harness=${harness:-unknown} model=${model:-unset} provider=${provider:-unset}" >&2
