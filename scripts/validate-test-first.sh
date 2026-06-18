#!/usr/bin/env bash
# Validate TDD discipline before running commands
#
# This script is called via PreToolUse hook when a archer agent
# attempts to run bash commands. It checks if tests have been run
# recently and warns if not.
#
# Usage: Add to agent frontmatter:
# hooks:
#   PreToolUse:
#     - matcher: "Bash"
#       hooks:
#         - type: command
#           command: "./scripts/validate-test-first.sh"

set -euo pipefail

# Read input from stdin (contains tool info)
INPUT=$(cat)

# Extract command being run
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // ""')

# Commands that are always allowed
ALLOWED_PATTERNS=(
    "npm test"
    "pnpm test"
    "bun test"
    "vitest"
    "pytest"
    "jest"
    "cargo test"
    "go test"
    "git"
    "ls"
    "cat"
    "echo"
    "pwd"
    "cd"
    "curl.*localhost"
    "docker.*test"
)

# Check if command matches allowed patterns
for pattern in "${ALLOWED_PATTERNS[@]}"; do
    if echo "$COMMAND" | grep -qE "$pattern"; then
        exit 0  # Allow
    fi
done

# Check for build/run commands that should have tests first
BUILD_PATTERNS=(
    "npm run build"
    "pnpm build"
    "npm run dev"
    "pnpm dev"
    "npm start"
    "yarn build"
)

# For build commands, enforce TDD discipline: tests must run before builds
for pattern in "${BUILD_PATTERNS[@]}"; do
    if echo "$COMMAND" | grep -qE "$pattern"; then
        if [[ ! -f ".fno/.last-test-run" ]]; then
            cat << 'EOF' >&2
───────────────────────────────────────────────
TDD REMINDER
───────────────────────────────────────────────
You're running a build/start command.

Have you verified tests pass first?
  pnpm test  or  npm test  or  vitest

TDD Flow: Test (FAIL) → Implement → Test (PASS) → Build
───────────────────────────────────────────────
EOF
            echo "ERROR: TDD violation — run tests before build commands" >&2
            exit 1
        fi
        # Test tracker exists — allow build
        break
    fi
done

# Allow other operations
exit 0
