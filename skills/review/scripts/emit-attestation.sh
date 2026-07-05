#!/usr/bin/env bash
# Emit a head-pinned review_attestation event (x-e703, Phase 2).
#
# This is the single producer surface for the config.review.reviewers gate: a
# local reviewer (sigma | code-review | declare) that leaves NO GitHub review
# object emits this event so `fno-agents loop-check` can read it as gate
# evidence. loop-check head-pins on the CURRENT HEAD - a pass on a prior commit
# stops counting the moment a new commit lands, so this MUST run after the
# reviewed commit is the tip.
#
# Usage: emit-attestation.sh <reviewer> [verdict]
#   <reviewer>  sigma | code-review | declare  (a leading '/' is stripped)
#   [verdict]   pass (default) | fail
set -euo pipefail

reviewer="${1:?reviewer name required (sigma|code-review|declare)}"
verdict="${2:-pass}"
while [[ "$reviewer" == /* ]]; do reviewer="${reviewer#/}"; done # strip ALL leading slashes (parity with both parsers' lstrip / trim_start_matches)

head_sha="$(git rev-parse HEAD 2>/dev/null)" || {
  echo "emit-attestation: not a git repo (cannot head-pin); no event emitted" >&2
  exit 1
}

# Build the data object with jq so a reviewer/verdict value can never break the
# JSON (codex peer review P2). fno event emit then validates envelope + required
# fields + the verdict enum before writing.
data="$(jq -cn --arg reviewer "$reviewer" --arg head_sha "$head_sha" --arg verdict "$verdict" \
  '{reviewer:$reviewer,head_sha:$head_sha,verdict:$verdict}')"
fno event emit -t review_attestation -s target -d "$data"

echo "review_attestation emitted: reviewer=$reviewer head_sha=${head_sha:0:8} verdict=$verdict" >&2
