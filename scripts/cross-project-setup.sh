#!/usr/bin/env bash
set -euo pipefail

# cross-project-setup.sh
# Creates a worktree in a target project, installs deps, runs baseline tests.
# Called by cross-project-pipeline skill's Setup step subagents.
#
# Thin wrapper around scripts/lib/worktree-manager.sh - kept as the public
# entry point for back-compat. Path resolution, branch naming, and setup
# caching all flow through the shared manager so per-project worktree_base
# (from settings.yaml) is honored.
#
# Usage: cross-project-setup.sh <project-path> <feature-slug> [setup-command] [test-command]

PROJECT_PATH="${1:?Usage: cross-project-setup.sh <project-path> <feature-slug> [setup-command] [test-command]}"
FEATURE_SLUG="${2:?Usage: cross-project-setup.sh <project-path> <feature-slug> [setup-command] [test-command]}"
SETUP_CMD="${3:-}"
TEST_CMD="${4:-}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
WTM="$SCRIPT_DIR/lib/worktree-manager.sh"

# ERR trap: emit JSON on any unhandled failure so callers can parse the error
trap 'echo "{\"status\": \"FAILED\", \"error\": \"Script failed at line $LINENO\"}" ; exit 1' ERR

# Validate project path
if [[ ! -d "$PROJECT_PATH" ]]; then
    echo "ERROR: Project path does not exist: $PROJECT_PATH" >&2
    echo '{"status": "FAILED", "error": "Project path does not exist: '"$PROJECT_PATH"'"}'
    exit 1
fi

# Resolve the project name. settings.yaml entries key off basename; we use
# the last path component, which matches the way detect_project_from_settings
# walks `path:` entries.
PROJECT_NAME="$(basename "$PROJECT_PATH")"

cd "$PROJECT_PATH"

# Verify it's a git repo
if ! git rev-parse --is-inside-work-tree &>/dev/null; then
    echo "ERROR: Not a git repository: $PROJECT_PATH" >&2
    echo '{"status": "FAILED", "error": "Not a git repository: '"$PROJECT_PATH"'"}'
    exit 1
fi

# Ensure .claude/worktrees is gitignored (back-compat: settings.yaml may not
# point worktree_base at .claude/worktrees, but if it does we want the dir
# excluded so worktree contents don't accidentally get staged).
if ! git check-ignore -q ".claude/worktrees" 2>/dev/null; then
    if ! git check-ignore -q ".claude" 2>/dev/null; then
        echo ".claude/worktrees" >> .gitignore
        git add .gitignore
        if ! git commit -m "chore: add .claude/worktrees to .gitignore" >&2; then
            echo "WARNING: could not commit .gitignore update" >&2
        fi
        echo "Added .claude/worktrees to .gitignore" >&2
    fi
fi

# Delegate worktree creation to the shared manager. It honors settings.yaml
# worktree_base for $PROJECT_NAME, falling back to .claude/worktrees.
CREATE_OUT=$(bash "$WTM" create "$PROJECT_NAME" "$FEATURE_SLUG" --mode=manual)
# Single python3 call extracts both fields - cross-project setup is a hot
# path for parallel project worktrees, so save the second interpreter start.
PARSED=$(echo "$CREATE_OUT" | python3 -c \
    'import json,sys; d=json.load(sys.stdin); print(d["path"]); print(d["branch"])' 2>/dev/null) || PARSED=""
WORKTREE_PATH=$(echo "$PARSED" | sed -n '1p')
BRANCH_NAME=$(echo "$PARSED" | sed -n '2p')

if [[ -z "$WORKTREE_PATH" ]]; then
    echo "ERROR: worktree-manager create failed: $CREATE_OUT" >&2
    echo '{"status": "FAILED", "error": "worktree-manager create failed"}'
    exit 1
fi

cd "$WORKTREE_PATH"
RESOLVED_WORKTREE_PATH=$(pwd)

# Install dependencies. If a custom setup command is provided we honor it
# directly (back-compat with old call sites); otherwise delegate to the
# manager's setup verb so we get lockfile-hash cache + env-file copy.
if [[ -n "$SETUP_CMD" ]]; then
    echo "Running setup: $SETUP_CMD" >&2
    if ! eval "$SETUP_CMD" >&2; then
        echo '{"status": "FAILED", "error": "Setup command failed: '"$SETUP_CMD"'"}'
        exit 1
    fi
else
    # Setup failure must propagate. Previously we logged a warning and let
    # the final JSON claim status: OK - downstream pipelines (cross-project)
    # then shipped half-installed worktrees thinking deps were ready.
    if ! bash "$WTM" setup "$RESOLVED_WORKTREE_PATH" >&2; then
        echo '{"status": "FAILED", "error": "worktree-manager setup failed (deps not installed)"}'
        exit 1
    fi
fi

# Run baseline tests (disable set -e for test section)
TEST_EXIT=0
if [[ -n "$TEST_CMD" ]]; then
    echo "Running tests: $TEST_CMD" >&2
    set +e
    eval "$TEST_CMD" >&2
    TEST_EXIT=$?
    set -e
    if [[ $TEST_EXIT -ne 0 ]]; then
        echo "WARNING: Baseline tests failed with exit code $TEST_EXIT" >&2
    fi
fi

# Output result as JSON (stdout only — all other output went to stderr).
# Schema preserved for back-compat with cross-project-pipeline skill callers.
cat <<EOF
{
  "status": "OK",
  "worktree_path": "$RESOLVED_WORKTREE_PATH",
  "branch": "$BRANCH_NAME",
  "test_exit": $TEST_EXIT
}
EOF
