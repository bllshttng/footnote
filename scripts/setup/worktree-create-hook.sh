#!/usr/bin/env bash
# Claude Code WorktreeCreate hook: canonical conductor location.
#
# Creates the worktree at `~/conductor/workspaces/<repo>/<name>` (repo name
# from the canonical checkout's directory basename) rather than at Claude
# Code's default `.claude/worktrees/<name>`, then runs `setup-worktree.sh`
# inside it.
#
# Wiring (pick one; do NOT pick both for the same repo - hooks merge in
# parallel and matching hooks are run concurrently, which races):
#
#   1. User-global (recommended for non-fno projects): point your
#      `~/.claude/settings.json` `hooks.WorktreeCreate` at this script. It
#      will redirect every `claude --worktree` invocation across every
#      project to its canonical conductor location.
#
#   2. Plugin-level (for fno-ecosystem projects): the fno
#      plugin's WorktreeCreate hook at `hooks/worktree-setup.sh` does the
#      same redirect when `worktree.use_conductor_canonical: true` is set
#      in `.fno/settings.yaml`. Prefer this over wiring this script
#      because the plugin hook also handles dep install, env copy, and
#      verification in one pass.
#
# See .claude/rules/worktrees.md for the full reconciliation table.
#
# Input (stdin):  Claude Code's WorktreeCreate payload. Only `name` is read;
#                 `baseRef` is NOT in the stdin schema (it's a settings.json
#                 key that controls Claude's own internal worktree creation,
#                 which this hook overrides anyway).
# Output (stdout): the directory path Claude Code will use as cwd.
# Output (stderr): progress messages.

set -euo pipefail

# Parse `name` from stdin. Claude Code's WorktreeCreate hook stdin schema
# contains common fields (session_id, transcript_path, cwd, hook_event_name)
# plus `name`. The `worktree.baseRef` setting (v2.1.133+) is Claude Code's
# own internal default for worktrees IT creates - it is NOT forwarded to
# custom hook stdin. A custom hook owns all branching decisions, so we
# always branch from origin/HEAD (with local-HEAD fallback) and ignore any
# baseRef-related config.
#
# python3 ships with macOS and every Linux distro this repo targets; jq is
# not guaranteed. The hook is in Claude Code's hot path; failing here means
# `claude --worktree <name>` cannot create a worktree at all.
INPUT="$(cat)"
NAME="$(printf '%s' "$INPUT" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except (json.JSONDecodeError, ValueError):
    d = {}
print(d.get("name", ""))
')"

if [ -z "$NAME" ]; then
    echo "WorktreeCreate hook: no name in input payload" >&2
    exit 1
fi

# Resolve repo name from the canonical checkout rather than hardcoding so
# this script is portable to other repos via user-global wiring (see
# .claude/rules/worktrees.md for the recipe).
MAIN_REPO="$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null | sed 's|/\.git$||')"
[ -n "$MAIN_REPO" ] || { echo "WorktreeCreate hook: not in a git repo" >&2; exit 1; }
REPO_NAME="$(basename "$MAIN_REPO")"
WORKTREE_PATH="$HOME/conductor/workspaces/$REPO_NAME/$NAME"
BRANCH_NAME="worktree-$NAME"

echo "=== Creating worktree at $WORKTREE_PATH ===" >&2
echo "    Branch: $BRANCH_NAME" >&2

# Refuse if the destination already exists; recreating would corrupt the
# existing worktree's git state. Operator can `git worktree remove` first.
if [ -e "$WORKTREE_PATH" ]; then
    echo "Worktree path already exists: $WORKTREE_PATH" >&2
    echo "Remove it first: git worktree remove --force $WORKTREE_PATH" >&2
    exit 1
fi

# Resolve the base ref. origin/HEAD is not guaranteed to be configured in
# every clone (shallow clones, renamed remotes, local-only clones), so
# fall back to local HEAD if the symbolic ref does not resolve.
git fetch origin >&2 2>/dev/null || true
if git rev-parse --verify --quiet origin/HEAD >/dev/null; then
    BASE="origin/HEAD"
else
    echo "    origin/HEAD not configured; falling back to local HEAD" >&2
    BASE="HEAD"
fi

# Recreate-after-remove flow: `git worktree remove` does NOT delete the
# branch. Without this check, re-running `claude --worktree X` after
# removing X fails with "a branch named worktree-X already exists". Attach
# to the existing branch; user can `git branch -D` to start fresh.
if git show-ref --verify --quiet "refs/heads/$BRANCH_NAME"; then
    echo "    Branch $BRANCH_NAME already exists; attaching (git branch -D $BRANCH_NAME to start fresh)" >&2
    git worktree add "$WORKTREE_PATH" "$BRANCH_NAME" >&2
else
    git worktree add -b "$BRANCH_NAME" "$WORKTREE_PATH" "$BASE" >&2
fi

# Run the canonical setup script inside the new worktree if it exists.
# The script handles internal/ symlink, .fno/ state linking, .claude/
# subdir symlinks, etc. - all fno-specific. When this hook is wired
# user-global (see .claude/rules/worktrees.md) and lands in a non-fno
# repo, the script is absent; treat that as a bare checkout and continue
# rather than aborting the hook (codex P1).
SETUP="$WORKTREE_PATH/scripts/setup/setup-worktree.sh"
if [ -x "$SETUP" ] || [ -f "$SETUP" ]; then
    (
        cd "$WORKTREE_PATH"
        bash scripts/setup/setup-worktree.sh
    ) >&2 || echo "Note: setup-worktree.sh exited non-zero; worktree usable but not fully bootstrapped" >&2
else
    echo "Note: no scripts/setup/setup-worktree.sh in this repo; leaving bare worktree" >&2
fi

# Last line of stdout is the working directory Claude Code uses.
echo "$WORKTREE_PATH"
