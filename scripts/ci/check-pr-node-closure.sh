#!/usr/bin/env bash
# check-pr-node-closure.sh - CI gate: a node-bearing branch must exact-claim
# its own node in the PR's Backlog-Closure trailer.
#
# x-59a6: a PR naming several backlog nodes only ever closed the ONE node
# individually stamped at creation; every other named node stayed open
# forever. The fix is an exact `Backlog-Closure: <id> [<id>...]` trailer,
# bound atomically at merge - this gate is its CI backstop for the direct
# `gh pr create` path, which never runs the `fno do pr closure-trailer`
# generator. It never infers extra nodes from prose or diffs: it only checks
# that a node id already present in the HEAD ref is also named in the exact
# trailer line.
#
# Run: PR_BODY="<body>" PR_HEAD_REF="<branch>" bash scripts/ci/check-pr-node-closure.sh
# Env: PR_BODY (the PR body), PR_HEAD_REF (the PR's head branch name).
# Exit: 0 pass or skip (non-node branch, no PR_HEAD_REF set), 1 missing claim.

set -euo pipefail

PR_BODY="${PR_BODY:-}"
PR_HEAD_REF="${PR_HEAD_REF:-}"

# Fail-open: no head ref to check (local run, or an event that carries none).
if [[ -z "$PR_HEAD_REF" ]]; then
  echo "check-pr-node-closure: no PR_HEAD_REF set, skipping."
  exit 0
fi

# Liberal FORMAT match, sourced from the one shell copy of the node-id shape
# (kept aligned with the Python source of truth by its own pinning test,
# test_node_id_sh.py) rather than a second hardcoded copy here that could
# silently drift. This is a format check, not an identity check: no graph is
# available in CI to confirm the id is real, so a branch segment that merely
# LOOKS like a node id (e.g. a coincidental "db-2026") is treated the same as
# a real one - the documented liberal-extraction tradeoff, not a bug.
_script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/node-id.sh
source "${_script_dir}/../lib/node-id.sh"
# _NODE_ID_FNO_RE is anchored (^...$); strip both anchors so this script's own
# `^${node_id_re}$` wrapping at the match site below stays the single place
# anchoring happens.
node_id_re="${_NODE_ID_FNO_RE#^}"
node_id_re="${node_id_re%\$}"

# Extract every delimiter-bounded candidate segment from the head ref. Split on
# '/' FIRST, then on '-' inside each path component, so each candidate is
# compared whole, not as a substring of a longer token (mirrors
# `_branch_matches_node`'s delimiter-bounded match, never a bare substring).
#
# The two splits stay separate because the re-glue below joins with a literal
# '-'. A single `IFS='/-'` split forgot WHICH delimiter it consumed, so it
# re-glued two segments that a '/' separated and demanded an id the branch
# never names: "feat/cafe" asked for "feat-cafe", "target/deadbeef" for
# "target-deadbeef". Those refs name no node, and the producer
# (fno.pr.closure.branch_node_ids, which requires a literal '-') writes no
# trailer for them - so the gate red a PR over a line nothing could generate.
candidates=()
IFS='/' read -ra _paths <<< "$PR_HEAD_REF"
for _path in "${_paths[@]}"; do
  IFS='-' read -ra _segments <<< "$_path"
  i=0
  while [[ $i -lt ${#_segments[@]} ]]; do
    # Re-glue two adjacent segments (the id's own prefix/suffix straddle the
    # '-' IFS split point: "x" and "59a6" from "feature/x-59a6").
    if [[ $((i + 1)) -lt ${#_segments[@]} ]]; then
      pair="${_segments[$i]}-${_segments[$((i + 1))]}"
      if [[ "$pair" =~ ^${node_id_re}$ ]]; then
        candidates+=("$pair")
        # Skip BOTH consumed segments, not just one: a real id's all-hex
        # suffix (e.g. "cdef" in "x-cdef") is itself a valid node-id PREFIX
        # shape, so sliding by one would re-glue it with the next segment
        # ("cdef-1234") and invent a second, bogus candidate. Reproduced
        # live: PR_HEAD_REF="feature/x-cdef-1234" used to demand a
        # "Backlog-Closure: cdef-1234" line that names nothing real.
        i=$((i + 2))
        continue
      fi
    fi
    i=$((i + 1))
  done
done

if [[ ${#candidates[@]} -eq 0 ]]; then
  echo "check-pr-node-closure: no node id in HEAD ref '$PR_HEAD_REF', skipping (non-node branch)."
  exit 0
fi

# The LAST exact Backlog-Closure line only (mirrors fno.pr.closure.parse_closure_trailer:
# a stale earlier line, e.g. carried forward by a rebase, must not satisfy this).
trailer_line=$(printf '%s\n' "$PR_BODY" | grep -iE '^Backlog-Closure:[[:space:]]*' | tail -1 || true)

# Strip the label itself (everything through its own colon + any immediate
# spaces/tabs), mirroring `_TRAILER_LINE_RE`'s `^Backlog-Closure:[ \t]*(.*)$`
# capture group - the runtime parser (parse_closure_trailer) only ever
# tokenizes THAT captured remainder, on whitespace and "," alone, never a
# bare ":". Matching against the raw trailer_line (label prefix still
# attached) let ANY colon in the line - including a stray one BETWEEN two
# ids, e.g. "Backlog-Closure:x-59a6:x-1111" - read as a valid separator via
# the leading-boundary group below, so the gate passed a trailer the real
# parser tokenizes as one malformed run and binds zero ids from (round-10
# review fix: reproduced live, gate passed / parser returned []).
trailer_body=""
if [[ "$trailer_line" =~ ^[^:]*:[[:space:]]*(.*)$ ]]; then
  trailer_body="${BASH_REMATCH[1]}"
fi

missing=()
for cand in "${candidates[@]}"; do
  # The preceding boundary also accepts "," - the runtime parser's
  # `.replace(",", " ")` before splitting treats a comma as an equivalent
  # separator with no space required either side. "x-59a6,x-1111" binds both
  # ids at merge time; without "," in the LEADING alternation this gate
  # reports the second id as missing even though it closes correctly
  # (round-8 fix). No ":" in this alternation - trailer_body already has the
  # label's own colon stripped, so any colon reaching here is a real
  # malformed token, not a separator.
  if ! printf '%s' "$trailer_body" | grep -qE "(^|[[:space:]]|,)${cand}([[:space:]]|,|\$)"; then
    missing+=("$cand")
  fi
done

# AT LEAST ONE claimed, never all of them. This gate has no graph (see the
# format-check note above), so it cannot tell a real node id from ordinary
# English that fits the same grammar. The producer CAN, and refuses to claim
# an id the graph does not carry, because one unknown id makes
# bind_closure_claims refuse the WHOLE binding at merge.
#
# Demanding all of them therefore made some branches unsatisfiable rather than
# merely strict: on "feature/x-49ec-cache-dead" the producer writes x-49ec and
# this gate demanded "cache-dead", so no body passed both. Reproduced live
# before this change. An unsatisfiable gate is worse than a liberal one - it
# has no green state, so the only way past it is to ignore it.
#
# One claim still catches the defect this gate exists for: a `gh pr create`
# that wrote no trailer at all names zero ids and fails here.
claimed=$(( ${#candidates[@]} - ${#missing[@]} ))
if [[ $claimed -eq 0 ]]; then
  {
    echo "check-pr-node-closure: HEAD ref '$PR_HEAD_REF' names $(IFS=,; echo "${candidates[*]}"), and the exact trailer claims none of them."
    echo "  Add a line reading:"
    echo "    Backlog-Closure: <the node id this PR closes>"
    echo "  Generate it with: fno do pr closure-trailer <node-id>, which checks the"
    echo "  id against the graph. Do NOT paste a candidate from this message:"
    echo "  a branch segment can match the id grammar without being a real node,"
    echo "  and one unknown id voids the whole binding at merge."
  } >&2
  exit 1
fi

if [[ ${#missing[@]} -gt 0 ]]; then
  echo "check-pr-node-closure: HEAD ref '$PR_HEAD_REF' claims $claimed of ${#candidates[@]} candidate(s); unclaimed: ${missing[*]} (not demanded - this gate reads no graph)."
else
  echo "check-pr-node-closure: HEAD ref '$PR_HEAD_REF' node id(s) [${candidates[*]}] all present in the exact trailer."
fi
