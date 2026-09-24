#!/usr/bin/env bash
set -euo pipefail

target="${1:-}"
[[ -n "$target" && "$target" != -* ]] || {
  echo "resolve-pr-worktree: expected a PR number or head branch" >&2
  exit 2
}

branch="$target"
if [[ "$target" =~ ^[1-9][0-9]*$ ]]; then
  branch="$(gh api "repos/{owner}/{repo}/pulls/${target}" --jq '.head.ref')" || {
    echo "resolve-pr-worktree: could not read PR ${target} head branch" >&2
    exit 3
  }
fi
[[ -n "$branch" ]] || {
  echo "resolve-pr-worktree: target ${target} returned no head branch" >&2
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
