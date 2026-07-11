#!/usr/bin/env bash
# tests/ci/test_oos_tracked.sh
#
# Exercises scripts/ci/check-oos-tracked.sh: the PR-body "Out of scope" gate.
# Pure PR_BODY-env in / exit-code out, no git repo needed.
#
# Run: bash tests/ci/test_oos_tracked.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$(cd "${SCRIPT_DIR}/../.." && pwd)/scripts/ci/check-oos-tracked.sh"

PASS=0; FAIL=0
[[ -f "$GATE" ]] || { echo "gate not found at $GATE" >&2; exit 1; }

# run <expected_exit> <label> <body> : assert the gate's exit code
run() {
  local want="$1" label="$2" body="$3" got
  PR_BODY="$body" bash "$GATE" >/dev/null 2>&1; got=$?
  if [[ "$got" -eq "$want" ]]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s\n  want exit %s, got %s\n' "$label" "$want" "$got"
  fi
}

# --- pass: no OOS section / empty body ---------------------------------------
run 0 'empty body'            ''
run 0 'no OOS heading'        $'## What\nsome change\n## Verification\nran tests'

# --- pass: OOS section with a tracked ref ------------------------------------
run 0 'node ref x- (paragraph)' $'## Out of scope\nTier-3 flags need CLI work, tracked as x-b6e2.'
run 0 'node ref ab-'            $'## Out of scope\nMigration cleanup - see ab-1234abcd.'
run 0 'carveout ref cv-'        $'## Out of scope\nFlaky test - cv-1383dc76 captures it.'

# --- fail: OOS section, bare item, no ref ------------------------------------
run 1 'bare paragraph, no ref'  $'## Out of scope\nTier-3 flags deliberately not touched here.'
run 1 'bare bullet, no ref'     $'## Out of scope\n- Tier-3 flags need CLI-side work'

# --- waivers -----------------------------------------------------------------
run 0 'standalone oos-ok waives section' $'## Out of scope\noos-ok: fully covered by the existing #340 change\n- some item with no ref'
run 0 'inline oos-ok on item'   $'## Out of scope\n- Migration cleanup oos-ok: already done in #340'
run 1 'bare oos-ok (no rationale) does NOT waive' $'## Out of scope\noos-ok:'
run 1 'inline bare oos-ok on item does not waive' $'## Out of scope\n- Migration cleanup oos-ok:'

# --- per-item: mixed list, one bullet untracked ------------------------------
run 1 'mixed bullets, one untracked' $'## Out of scope\n- Tier-3 flags - tracked as x-b6e2\n- Migration cleanup with no ref'
run 0 'all bullets tracked'          $'## Out of scope\n- Tier-3 flags - x-b6e2\n- Migration cleanup - ab-1234abcd'

# --- heading variants + section boundary -------------------------------------
run 1 '"Not touched here" heading gated' $'### Not touched here\nthe frobnicator rewrite'
run 0 'untracked prose AFTER a later heading is NOT gated' \
  $'## Out of scope\nrefactor - x-b6e2\n## Notes\nthis deliberately not touched prose is outside the section'
run 1 'out-of-scope with hyphen spelling' $'## Out-of-scope\nthe thing we skipped'

# --- empty section is a no-op ------------------------------------------------
run 0 'empty OOS section' $'## Out of scope\n\n## Verification\nran tests'

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
