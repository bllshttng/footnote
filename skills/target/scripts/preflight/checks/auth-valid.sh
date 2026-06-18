#!/usr/bin/env bash
# auth-valid.sh - Check gh CLI authentication status
# Contract: stdout one line "auth-valid {pass|fail|warn|unknown} {message}"
# Exit: always 0
# Note: project-conditional - skips if gh is not in the project's toolchain

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"

# Check if gh is installed
if ! command -v gh >/dev/null 2>&1; then
    echo "auth-valid unknown gh CLI not installed (install from https://cli.github.com/)"
    exit 0
fi

# Check if this looks like a GitHub project (has a remote or .github/ dir)
HAS_GITHUB_REMOTE=0
if git remote -v 2>/dev/null | grep -q "github.com"; then
    HAS_GITHUB_REMOTE=1
fi
HAS_GITHUB_DIR=0
if [[ -d "$REPO_ROOT/.github" ]]; then
    HAS_GITHUB_DIR=1
fi

if [[ $HAS_GITHUB_REMOTE -eq 0 && $HAS_GITHUB_DIR -eq 0 ]]; then
    echo "auth-valid unknown project does not appear to use GitHub (no github.com remote or .github/)"
    exit 0
fi

# Run gh auth status
if gh auth status >/dev/null 2>&1; then
    # Extract the logged-in user for context
    USER_INFO=$(gh auth status 2>&1 | grep "Logged in" | head -1 | sed 's/.*Logged in to //' | sed 's/ as /: @/' | sed 's/ (.*//' || echo "authenticated")
    echo "auth-valid pass gh authenticated ($USER_INFO)"
else
    echo "auth-valid fail gh not authenticated - run: gh auth login"
fi
exit 0
