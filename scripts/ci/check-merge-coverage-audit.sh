#!/usr/bin/env bash
# check-merge-coverage-audit.sh - assert the merge coverage gate is still real.
#
# Two POSITIVE assertions, both required:
#   1. The ruleset still exists and still matches the committed data
#      (apply-merge-ruleset.sh --check). A required check protects the
#      branch; nothing protects the required check - this audit is the only
#      thing that would notice it being deleted or weakened in the UI.
#   2. Every merge commit in range landed with a SUCCESS `fno/review-coverage`
#      status on its second parent (the merged head). This asserts the marker
#      that a covered verdict existed - never the absence of a refusal: an
#      absence has two explanations, and only one of them is the outcome.
#      A merge whose head carries the override marker passes and is reported
#      BY NAME, so the release valve is counted, not hidden.
#
# A first-parent commit with no second parent (a direct push that predates
# the ruleset) is skipped with a named line, never silently. Squash and rebase
# merges have no second parent either, so this audit covers MERGE commits -
# the strategy this repo's auto_merge config uses; a repo retargeting the
# script at a squash workflow must key the walk on the PR API instead.
#
# Usage: check-merge-coverage-audit.sh --since <git-range-start>   (full audit)
#        check-merge-coverage-audit.sh --status-only <sha>         (assertion 2 only)
# Exit:  0 every assertion holds
#        1 an assertion failed - the lines above name which
#        2 usage error
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CTX="fno/review-coverage"

mode=""
since=""
sha=""
while [ $# -gt 0 ]; do
  case "$1" in
    --since) mode="audit"; since="${2:?--since needs a ref}" ; shift 2 ;;
    --status-only) mode="status"; sha="${2:?--status-only needs a sha}" ; shift 2 ;;
    *)
      echo "usage: $0 --since <ref> | --status-only <sha>" >&2
      exit 2
      ;;
  esac
done
if [ -z "$mode" ]; then
  echo "usage: $0 --since <ref> | --status-only <sha>" >&2
  exit 2
fi

short() { printf '%s' "$1" | cut -c1-8; }

# The latest state and description of the coverage context on one sha.
# Empty state means absent - which the caller treats as a failure, so an API
# hiccup can never read as a pass (fail closed, like everything else here).
#
# The COMBINED endpoint, never the status list. The list is newest-first with
# one-second `updated_at` granularity, and jq's sort_by is stable, so
# `sort_by(.updated_at) | last` returns the OLDEST member of a same-second
# tie. A refresher posting failure and a publisher posting success within the
# same second would read as failure and red main for a covered merge, with
# the branch already gone and nothing able to repair it. The combined
# endpoint returns exactly one entry per context, the latest, so there is no
# tie to break.
coverage_field() { # <sha> <jq-tail: .state or .description>
  gh api "repos/:owner/:repo/commits/$1/status" \
    --jq "[.statuses[] | select(.context == \"$CTX\")] | first | $2 // empty" \
    2>/dev/null || true
}

audit_head() { # <head-sha> <label-for-messages>
  local head="$1" label="$2"
  local state desc
  state="$(coverage_field "$head" '.state')"
  if [ "$state" = "success" ]; then
    desc="$(coverage_field "$head" '.description')"
    case "$desc" in
      coverage-override*)
        echo "override: $label merged on the override marker ($desc)"
        return 0
        ;;
    esac
    echo "ok: $label head $(short "$head") carries success ${CTX}"
  else
    echo "FAIL: $label landed head $(short "$head") with ${CTX} state '${state:-absent}' - no covered verdict at the merged head" >&2
    return 1
  fi
}

if [ "$mode" = "status" ]; then
  audit_head "$sha" "requested sha"
  exit $?
fi

# Assertion 1: the gate itself. The applier's own output names a drifted
# expectation; this line only adds which half of the audit failed.
if ! bash "$HERE/apply-merge-ruleset.sh" --check; then
  echo "FAIL: assertion 1 - the merge ruleset no longer matches the committed data" >&2
  exit 1
fi

if ! git rev-parse --verify -q "$since" >/dev/null 2>&1; then
  echo "FAIL: --since ref '$since' does not resolve in this checkout" >&2
  exit 2
fi

# Assertion 2: the positive marker on every merge in range.
fail=0
merges=0
skipped=0
# Captured and status-checked BEFORE the loop: a `git log` failure inside a
# process substitution feeds the loop nothing, and "audited 0 merges" would
# read as a pass. An audit that could not read history has audited nothing.
commits="$(git log --first-parent --format=%H "${since}..HEAD")" || {
  echo "FAIL: could not read history ${since}..HEAD" >&2
  exit 1
}
while read -r commit; do
  if head="$(git rev-parse --verify -q "$commit^2" 2>/dev/null)" && [ -n "$head" ]; then
    merges=$((merges + 1))
    audit_head "$head" "merge $(short "$commit")" || fail=1
  else
    skipped=$((skipped + 1))
    echo "skip: $(short "$commit") is a first-parent-only commit (direct push, predates the merge ruleset)"
  fi
done <<<"$commits"

echo "audited ${merges} merge(s) from $(short "$since") to $(short HEAD); ${skipped} first-parent-only commit(s) skipped by name"
if [ "$fail" = 1 ]; then
  echo "FAIL: assertion 2 - a merge landed without a covered verdict at its head" >&2
  exit 1
fi
echo "PASS: the merge coverage gate is present and every in-range merge carried a covered verdict"
