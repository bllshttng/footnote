#!/usr/bin/env bash
# Post the `stacked-base-guard` commit status for one PR.
#
# Refuses a PR whose base branch no longer leads to the default branch: merging
# it reports MERGED while the commits never reach main. The verdict comes from
# `fno pr base-lineage-check`, the same predicate `fno pr merge`, `fno pr
# verify` and the Rust auto-merge arm call, so CI and the CLI cannot disagree.
#
# Both callers write the SAME status context on purpose. A check-run name and a
# status context are different things to branch protection, so if the two jobs
# reported under different names an operator could only ever mark one of them
# required - and the push-triggered sweep, the one that catches a base dying
# after the PR's last push, would be the one left unable to block.
#
# Usage: check-stacked-base.sh <pr-number> <head-sha>
# Exit:  0 base still leads to the default branch AND the status was posted
#        1 stale base, the probe could not evaluate, or the status POST failed
#
# CI fails closed on an unevaluated probe: a check that could not run has
# verified nothing, and reporting it green is the defect this guard exists for.
# The in-process CLI callers deliberately do the opposite (proceed with a
# breadcrumb) so a gh outage cannot wedge a merge; the split is intentional.
set -uo pipefail

PR="${1:?usage: check-stacked-base.sh <pr-number> <head-sha>}"
SHA="${2:?usage: check-stacked-base.sh <pr-number> <head-sha>}"
CONTEXT="stacked-base-guard"

out="$(uv run --project cli fno-py pr base-lineage-check "$PR" 2>&1)"
rc=$?
printf '%s\n' "$out"

case "$rc" in
  0) state=success; desc="base still leads to the default branch" ;;
  3) state=failure; desc="base branch no longer leads to the default branch" ;;
  *) state=failure; desc="lineage check could not evaluate (exit $rc)" ;;
esac

# The status is the durable artifact: a job's own conclusion is scoped to that
# run, while a status stays attached to the head SHA where branch protection
# and the merge button read it. A failed POST is therefore a failed check, not
# a warning: the job would otherwise go green having published nothing for
# branch protection to read - the same "verified nothing, reported fine" shape
# the unevaluated-probe case above already fails closed on.
if ! gh api --method POST "repos/${GITHUB_REPOSITORY}/statuses/${SHA}" \
  -f state="$state" \
  -f context="$CONTEXT" \
  -f description="${desc:0:139}" \
  -f target_url="${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID}" \
  >/dev/null; then
  echo "FAIL: could not post the $CONTEXT status for $SHA (verdict was $state)" >&2
  exit 1
fi

[ "$state" = success ]
