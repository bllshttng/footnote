#!/usr/bin/env bash
# scripts/ci/lib/gate-fresh-common.sh
#
# The shape shared by the cross-tree freshness gates: repo-root resolution,
# --write/--worktree argument parsing, and the committed-versus-worktree blob
# selection. Sourced, never executed.
#
# It exists because there are now two of these gates and the shape is exactly
# the kind of hand-maintained second copy the change that added the second one
# argues against. A policy change to the committed-bytes fallback has to land
# in one place, not once per gate.
#
# Usage:
#   source "$(dirname "${BASH_SOURCE[0]}")/lib/gate-fresh-common.sh"
#   gate_resolve_repo_root          # sets REPO_ROOT, or exits 2
#   gate_parse_mode "$@"            # sets GATE_MODE to committed|worktree|write
#   gate_committed_blob <rel> <dest>  # rc 0 wrote the HEAD blob, rc 1 unavailable

# Resolve REPO_ROOT defensively, exiting 2 when there is no repo.
#
# The naive $(git rev-parse ...) inside command substitution can propagate
# git's rc=128 silently when bash runs with inherit_errexit (seen on GitHub
# Actions ubuntu-latest with bash 5.x). The explicit `if ! ...; then` form
# contains the failure regardless of inherit_errexit semantics.
gate_resolve_repo_root() {
  REPO_ROOT=""
  local git_root candidate
  if git_root=$(git rev-parse --show-toplevel 2>/dev/null); then
    REPO_ROOT="$git_root"
  fi
  if [[ -z "$REPO_ROOT" ]]; then
    # Fallback: walk up from the sourcing script's location looking for .git.
    candidate="$(cd "$(dirname "${BASH_SOURCE[1]}")" && pwd)"
    while [[ "$candidate" != "/" && "$candidate" != "." ]]; do
      if [[ -e "$candidate/.git" ]]; then
        REPO_ROOT="$candidate"
        break
      fi
      candidate="$(dirname "$candidate")"
    done
  fi
  if [[ -z "$REPO_ROOT" ]]; then
    echo "ERROR: not in a git repo (git rev-parse failed and no .git found via script-dir walk-up)" >&2
    exit 2
  fi
}

# Set GATE_MODE from the caller's arguments. An unknown argument exits 2.
# `--help` prints the caller's own header block and exits 0.
gate_parse_mode() {
  GATE_MODE="committed"
  local arg
  for arg in "$@"; do
    case "$arg" in
      --write) GATE_MODE="write" ;;
      --worktree) GATE_MODE="worktree" ;;
      -h|--help)
        sed -n '2,18p' "${BASH_SOURCE[1]}"
        exit 0
        ;;
      *)
        echo "ERROR: unknown argument: $arg" >&2
        echo "usage: $(basename "${BASH_SOURCE[1]}") [--write|--worktree]" >&2
        exit 2
        ;;
    esac
  done
}

# Write the HEAD blob for repo-relative path $1 into file $2.
#
# rc 0 means the committed bytes are in $2. rc 1 means there is no such blob
# (an unborn branch, or a path not yet tracked), and the caller compares the
# worktree instead. A repo with nothing committed has no committed drift to
# find, and passing silently would be worse than saying so, so the caller
# prints the degrade.
gate_committed_blob() {
  git -C "$REPO_ROOT" show "HEAD:$1" > "$2" 2>/dev/null
}
