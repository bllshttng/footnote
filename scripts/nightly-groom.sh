#!/usr/bin/env bash
# Nightly backlog groomer (node x-c2e9).
#
# A THIN wrapper that sequences already-shipped verbs - no grooming logic is
# reimplemented here. It splits work by RISK:
#   - reversible + mechanical (no judgment) -> auto-applied nightly
#       relatedness build   (sidecar, not a graph mutation)
#       archive --apply     (age-gated, reversible to graph-archive.json)
#       reconcile           (close nodes whose PR provably merged)
#       maintain --apply    (deterministic legs only)
#   - judgment (a wrong call loses work) -> PROPOSAL-ONLY, collected into a
#       timestamped digest, never applied (maintain's dedup/stale-defer/cap legs
#       stay proposal-only regardless of --apply).
#
# Best-effort: one failing leg is logged and does not abort the rest.
#
# Schedule:  /loop 1d bash scripts/nightly-groom.sh
#   or a `fno schedule` cron entry running the same line.
#
# First run (Open Q5): review the blast radius with --dry-run first, and clear
# the terminal backlog once with a wider gate before holding at steady state:
#   bash scripts/nightly-groom.sh --dry-run          # review, mutate nothing
#   fno backlog archive --apply --older-than-days 0   # one-time backfill
set -u

DRY_RUN=0
AGE=14   # steady-state archive age gate (Open Q4); operator tunes.

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run|-n) DRY_RUN=1 ;;
    --age) shift; AGE="${1:?--age needs a value}" ;;
    --age=*) AGE="${1#*=}" ;;
    -h|--help) echo "usage: nightly-groom.sh [--dry-run] [--age N]"; exit 0 ;;
    *) echo "nightly-groom: unknown arg '$1'" >&2; exit 2 ;;
  esac
  shift
done

# Resolve STATE_DIR canonically (never hardcode ~/.fno).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
source "$SCRIPT_DIR/lib/paths.sh"
DIGEST="$STATE_DIR/groom-digest.md"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
FNO="${FNO:-fno}"

run() {  # run a leg best-effort; a failure is logged, never aborts the rest.
  local label="$1"; shift
  echo ">> $label"
  local rc=0
  "$@" || rc=$?
  # 4 is this CLI's "nothing to do" code (e.g. reconcile found nothing to
  # close) - a normal quiet night, not a failure. Anything else is real.
  if [[ "$rc" == 0 || "$rc" == 4 ]]; then echo "   ok"; else echo "   FAILED (exit $rc) - continuing" >&2; fi
}

echo "== nightly-groom $TS (dry_run=$DRY_RUN age=${AGE}d) =="

if [[ "$DRY_RUN" == 1 ]]; then
  run "archive (dry-run)"   $FNO backlog archive --older-than-days "$AGE"
  run "reconcile (dry-run)" $FNO backlog reconcile --dry-run
  MAINTAIN_OUT="$($FNO backlog maintain 2>&1 || true)"
  # Do NOT build in dry-run: build writes the PRODUCTION sidecar, which would
  # change offer/triage results on a supposed review-only run.
  echo ">> relatedness build: skipped in --dry-run (would overwrite the production sidecar)"
else
  run "archive --apply"   $FNO backlog archive --apply --older-than-days "$AGE"
  run "reconcile --apply" $FNO backlog reconcile
  MAINTAIN_OUT="$($FNO backlog maintain --apply 2>&1 || true)"
  # Build LAST, after the mutating legs, so the map reflects the post-groom
  # graph - nodes archived this run are gone from the corpus, not left as
  # dangling edges until the next build.
  run "relatedness build" $FNO backlog relatedness build
fi
echo "$MAINTAIN_OUT"

# Digest: append the judgment-only proposals under a timestamped header so a
# stale/failed run is VISIBLE (a run that stops refreshing must not be silent).
{
  echo ""
  echo "## groom run $TS (age=${AGE}d, dry_run=$DRY_RUN)"
  echo '```'
  echo "$MAINTAIN_OUT"
  echo '```'
} >> "$DIGEST"
echo "digest appended -> $DIGEST"
