#!/usr/bin/env bash
set -euo pipefail

target="${1:-}"
[[ -n "$target" && "$target" != -* ]] || {
  echo "resolve-pr-worktree: expected a PR number or head branch" >&2
  exit 2
}
request="$(jq -cn --arg cwd "$PWD" --arg target "$target" \
  '{cwd:$cwd} + (if ($target | test("^[1-9][0-9]*$")) then {pr:($target|tonumber)} else {branch:$target} end)')"
response="$(printf '%s\n' "$request" | "${FNO_AGENTS_BIN:-fno-agents}" pr-worktree)" || {
  echo "resolve-pr-worktree: Rust worktree resolver refused target ${target}" >&2
  exit 3
}
worktree="$(printf '%s\n' "$response" | jq -er '.worktree')" || {
  echo "resolve-pr-worktree: resolver returned no worktree for ${target}" >&2
  exit 4
}
[[ -d "$worktree" ]] || {
  echo "resolve-pr-worktree: resolved worktree is unavailable" >&2
  exit 5
}
printf '%s\n' "$worktree"
