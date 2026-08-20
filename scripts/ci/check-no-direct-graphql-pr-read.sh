#!/usr/bin/env bash
# check-no-direct-graphql-pr-read.sh - every GitHub GraphQL / `gh pr` read is
# routed, not raw.
#
# The per-USER GraphQL quota is shared by every session on the machine, so one
# fleet of hand-rolled pollers starves the merge guard for everyone. This gate
# holds the routing boundary: reads go through the fno process proxy or a
# fixed-purpose adapter binary, never a bare `gh api graphql` in a hot loop.
#
# Why the probes look the way they do. Until 2026-08-20 nothing invoked this
# script, and it had been red on main for an unknown time. Neither failure was
# a lost boundary. Two of three probes pinned a whole assignment line, so an
# ordinary refactor (`worker_environment(os.environ)` -> `worker_environment(base)`,
# `let mut gh_bin = ...` -> an env-var lookup) broke them while the boundary
# stood. A guard that fails on refactors nobody can predict is a guard people
# learn to ignore. Each probe now names the SYMBOL that IS the boundary, so it
# fails when the boundary moves and survives when only its spelling does.
#
# The whole-inventory sha256 pin is gone for the same reason. It covered 194
# rows, 46 of them documentation, so rewording a comment re-broke it and the
# fix was always to re-paste a hex string nobody could read. What it actually
# bought over the unclassified assertion was detection of a NEW caller inside
# the two blanket-allow buckets (cli/src -> fno-process-proxy, skills/ ->
# worker-proxy-or-hook). A per-disposition count ceiling buys exactly that and
# nothing else: a new caller raises a count, a comment edit does not.
#
# Run:  bash scripts/ci/check-no-direct-graphql-pr-read.sh [--print-counts]
# Exit: 0 the boundary holds, 1 a violation.

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

BASELINE="scripts/ci/graphql-caller-baseline.txt"

matches=$(python3 scripts/ci/graphql-caller-inventory.py)

classified=$(printf '%s\n' "$matches" | awk 'NF {n++} END {print n+0}')
unclassified=$(printf '%s\n' "$matches" | awk -F'|' '$1 == "unclassified" {n++} END {print n+0}')

# Prose is excluded from the ceiling by construction, on two tests rather than
# one. The `documentation` disposition only catches a #- or //-prefixed line in
# a code file, so it misses markdown entirely: measured on this tree, all 11
# worker-proxy-or-hook rows were prose in skills/*.md, and pinning that bucket
# would have gone red the next time somebody wrote `gh pr view` in a skill doc.
# A .md file cannot execute, so it cannot be a caller. The disposition taxonomy
# is left alone; only the counting excludes it.
counts=$(printf '%s\n' "$matches" \
    | awk -F'|' 'NF && $1 != "documentation" && $3 !~ /\.md$/ {n[$1]++} END {for (k in n) print k, n[k]}' \
    | LC_ALL=C sort)

if [[ "${1:-}" == "--print-counts" ]]; then
  printf 'classified_graphql_callers=%s unclassified=%s\n' "$classified" "$unclassified"
  printf '%s\n' "$counts"
  exit 0
fi

if [[ "$classified" -eq 0 ]]; then
  echo "direct-graphql-pr-read: classification instrument matched zero callers" >&2
  exit 1
fi
if [[ "$unclassified" -ne 0 ]]; then
  printf '%s\n' "$matches" | awk -F'|' '$1 == "unclassified"' >&2
  echo "direct-graphql-pr-read: executable callers lack a routing disposition" >&2
  exit 1
fi
if ! printf '%s\n' "$matches" | grep -q '^fno-process-proxy|argv-pr|cli/src/fno/pr/_merge.py|'; then
  echo "direct-graphql-pr-read: argv inventory missed _merge.py" >&2
  exit 1
fi

# Pin the BOUNDARY, never the line that happens to express it today. Both of
# the first two pins broke on ordinary edits that changed nothing about the
# boundary: `worker_environment(os.environ)` became `worker_environment(base)`,
# and the `gh_bin` default was wrapped across two lines. A whole-line grep then
# reports "a named enforcement boundary is missing" for a boundary that is
# present and working, which is a false red nobody can act on. Match the
# assignment and the callee, or the binary name, and let formatting move.
#
# One `if` per boundary, not three in one condition: a single combined test can
# only ever say "a named enforcement boundary is missing", and the reader then
# has to bisect three greps to learn which.
if ! grep -q 'protected = worker_environment(' cli/src/fno/cli.py; then
  echo "direct-graphql-pr-read: cli.py no longer binds worker_environment() - the worker env boundary is gone" >&2
  exit 1
fi
# Anchored to the SPAWN and the env override, not to a bare string. Both files
# also name their adapter inside #[cfg(test)] blocks, so a bare grep would stay
# green on the test occurrence alone after the production boundary was deleted.
if ! grep -q 'Command::new("fno-gh-coverage")' crates/fno-agents/src/finalize.rs; then
  echo "direct-graphql-pr-read: finalize.rs no longer SPAWNS the fno-gh-coverage adapter" >&2
  exit 1
fi
if ! grep -q 'FNO_LOOPCHECK_GH_BIN' crates/fno-agents/src/loopcheck.rs; then
  echo "direct-graphql-pr-read: loopcheck.rs no longer resolves its gh binary through FNO_LOOPCHECK_GH_BIN" >&2
  exit 1
fi

if [[ ! -f "$BASELINE" ]]; then
  echo "direct-graphql-pr-read: baseline missing at $BASELINE" >&2
  exit 1
fi

# A ceiling, not equality: removing a caller is always allowed, adding one to a
# blanket-allow bucket is the thing to look at.
fail=0
while read -r disposition count; do
  [[ -n "$disposition" ]] || continue
  allowed=$(awk -v k="$disposition" '$1 == k {print $2}' "$BASELINE")
  if [[ -z "$allowed" ]]; then
    echo "direct-graphql-pr-read: disposition '$disposition' is not in $BASELINE - add it with its count and say why the callers are routed" >&2
    fail=1
  elif [[ "$count" -gt "$allowed" ]]; then
    echo "direct-graphql-pr-read: '$disposition' rose from $allowed to $count - a new caller landed in a blanket-allow bucket; route it or raise the baseline deliberately" >&2
    fail=1
  fi
done <<< "$counts"

[[ "$fail" -eq 0 ]] || exit 1

echo "direct-graphql-pr-read: $classified callers, 0 unclassified, every disposition at or under baseline"
