#!/usr/bin/env bash
set -euo pipefail

pr="${1:-}"
[[ "$pr" =~ ^[1-9][0-9]*$ ]] || {
  echo "resolve-pr-worktree: expected a positive PR number" >&2
  exit 2
}

branch="$(gh api "repos/{owner}/{repo}/pulls/${pr}" --jq '.head.ref')" || {
  echo "resolve-pr-worktree: could not read PR ${pr} head branch" >&2
  exit 3
}
[[ -n "$branch" ]] || {
  echo "resolve-pr-worktree: PR ${pr} returned no head branch" >&2
  exit 3
}

listing="$(git worktree list --porcelain)" || {
  echo "resolve-pr-worktree: git worktree list failed" >&2
  exit 4
}
worktree="$(printf '%s\n' "$listing" | awk -v branch="refs/heads/${branch}" '
  /^worktree / { path = substr($0, 10) }
  $1 == "branch" && $2 == branch { print path; exit }
')"
[[ -n "$worktree" && -d "$worktree" ]] || {
  echo "resolve-pr-worktree: no local worktree on PR branch ${branch}" >&2
  exit 5
}
printf '%s\n' "$worktree"
