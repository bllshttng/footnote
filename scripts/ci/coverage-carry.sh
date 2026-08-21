#!/usr/bin/env bash
# scripts/ci/coverage-carry.sh - decide whether a review verdict already on
# the previous head can be carried forward onto a new head whose diff is
# provably identical (a rebase or force-push that changed no code).
#
# A patch-id match proves the CODE is identical. It never proves a bot
# reviewed it. So this script only ever MOVES an existing publisher verdict
# forward - it never manufactures one. Every read failure and every
# ambiguous input falls through to no-carry; the caller then posts today's
# failure.
#
# Usage: coverage-carry.sh --repo <owner/repo> --context <ctx> \
#          --before <sha> --head <sha> --base <ref>
#
# Prints exactly one line to stdout and exits 0 for every DECISION (a usage
# error exits 2):
#   carry <origin-sha> <short-pid> <description-to-post>
#   no-carry <reason>
#
# The decision is the stdout TOKEN, never the exit code - an exit code
# answers "did the script run", and the caller needs "what did it find".
# Reasons: no-previous-head, status-read-failed, not-a-publisher-verdict:X,
# no-diff-identity, code-changed.
set -euo pipefail

repo="" ctx="" before="" head="" base=""
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) repo="${2:?--repo needs owner/repo}"; shift 2 ;;
    --context) ctx="${2:?--context needs a context string}"; shift 2 ;;
    --before) before="${2:?--before needs a sha}"; shift 2 ;;
    --head) head="${2:?--head needs a sha}"; shift 2 ;;
    --base) base="${2:?--base needs a ref}"; shift 2 ;;
    *)
      echo "usage: $0 --repo <owner/repo> --context <ctx> --before <sha> --head <sha> --base <ref>" >&2
      exit 2
      ;;
  esac
done
if [ -z "$repo" ] || [ -z "$ctx" ] || [ -z "$before" ] || [ -z "$head" ] || [ -z "$base" ]; then
  echo "usage: $0 --repo <owner/repo> --context <ctx> --before <sha> --head <sha> --base <ref>" >&2
  exit 2
fi

no_carry() { echo "no-carry $1"; exit 0; }

ZERO_SHA="0000000000000000000000000000000000000000"
if [ "$before" = "$ZERO_SHA" ] || [ "$before" = "$head" ]; then
  no_carry "no-previous-head"
fi

# The combined-status endpoint returns one latest entry per context. The
# status list is newest-first with one-second granularity, so a
# same-second tie needs the combined endpoint, not the list). One request
# returns both fields tab-separated, rather than two round trips for the
# same already-fetched response.
status_read() { # <sha> -> prints "state<TAB>description"
  local attempt out
  for attempt in 1 2 3; do
    if out="$(gh api "repos/${repo}/commits/${1}/status" \
      --jq "[.statuses[] | select(.context == \"${ctx}\")] | first | ((.state // \"\") + \"\t\" + (.description // \"\"))")"; then
      printf '%s' "$out"
      return 0
    fi
    echo "coverage-carry: status read attempt ${attempt} for ${1:0:8} failed; retrying after a backoff" >&2
    sleep 5
  done
  return 1
}

if ! status_out="$(status_read "$before")"; then
  no_carry "status-read-failed"
fi
IFS=$'\t' read -r before_state before_desc <<<"$status_out"
if [ "$before_state" != "success" ]; then
  no_carry "no-previous-head"
fi

# The publisher allowlist: EXACTLY the workflow's own preserve list, no
# wider. Refuses all three spellings of coverage-override* by name - a green
# the override label bought must never survive its own withdrawal by being
# carried onto the next head.
case "$before_desc" in
  covered*|"no review lane"*) ;;
  *)
    prefix="${before_desc%% *}"
    no_carry "not-a-publisher-verdict:${prefix:-empty}"
    ;;
esac

# Both identities come from GitHub's own rendered diff - no checkout of the
# repository being compared. Three-dot compare semantics mean the answer
# does not move when the base ref moves under it.
patch_id() { # <sha>
  # Same 3x/5s-backoff retry as status_read above, so a transient 5xx on the
  # compare call does not read as "the diff really is empty" (a real gap a
  # local /code-review pass caught: every sibling gh-api call in this file
  # retries, this one did not).
  local attempt out
  for attempt in 1 2 3; do
    if out="$(gh api "repos/${repo}/compare/${base}...${1}" \
      -H 'Accept: application/vnd.github.v3.diff' 2>/dev/null \
      | git patch-id --stable 2>/dev/null | cut -d' ' -f1)"; then
      printf '%s' "$out"
      return 0
    fi
    echo "coverage-carry: compare read attempt ${attempt} for ${1:0:8} failed; retrying after a backoff" >&2
    sleep 5
  done
  # Exhausted retries: fall through to an empty identity rather than abort -
  # the caller's `set -e` treats a failing command-substitution assignment as
  # fatal, and the no-diff-identity check below already treats an empty
  # identity as a safe no-carry.
  true
}

before_pid="$(patch_id "$before")"
head_pid="$(patch_id "$head")"

# Two empty diffs must never match - the same trap review_freshness names at
# loopcheck.rs:1301, where twelve empty diffs hashed to e3b0c442 and compared
# equal to each other.
if [ -z "$before_pid" ] || [ -z "$head_pid" ]; then
  no_carry "no-diff-identity"
fi

if [ "$before_pid" != "$head_pid" ]; then
  no_carry "code-changed"
fi

# Strip any existing "[carried from ...]" suffix first, so a chain of
# rebases never grows an unbounded suffix, then append one fresh marker,
# truncated to GitHub's 140-character description limit.
stripped="$(printf '%s' "$before_desc" | sed -E 's/ \[carried from [^]]*\]$//')"
marker=" [carried from ${before:0:8}]"
budget=$((140 - ${#marker}))
desc="${stripped:0:$budget}${marker}"

echo "carry ${before} ${head_pid:0:8} ${desc}"
