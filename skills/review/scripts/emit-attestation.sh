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
reviewer="${reviewer#/}" # normalize a leading slash, parity with both parsers

head_sha="$(git rev-parse HEAD 2>/dev/null)" || {
  echo "emit-attestation: not a git repo (cannot head-pin); no event emitted" >&2
  exit 1
}

# fno event emit validates the envelope + required data fields before writing.
fno event emit -t review_attestation -s target \
  -d "{\"reviewer\":\"$reviewer\",\"head_sha\":\"$head_sha\",\"verdict\":\"$verdict\"}"

echo "review_attestation emitted: reviewer=$reviewer head_sha=${head_sha:0:8} verdict=$verdict" >&2
