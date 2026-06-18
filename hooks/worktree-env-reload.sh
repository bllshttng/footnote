#!/usr/bin/env bash
# CwdChanged/FileChanged hook: reload env vars for worktree-aware execution
#
# When Claude cd's into a worktree or a .env file changes,
# this hook writes the correct env vars to CLAUDE_ENV_FILE
# so subsequent Bash commands use worktree-specific config.
#
# Supports direnv (preferred) or manual .env parsing (fallback).
set -uo pipefail

# CLAUDE_ENV_FILE is set by CC - it's the file CC sources before each Bash command
[[ -n "${CLAUDE_ENV_FILE:-}" ]] || exit 0

# If direnv is available and .envrc exists, use it (handles everything)
if command -v direnv >/dev/null 2>&1 && [[ -f ".envrc" ]]; then
    : > "$CLAUDE_ENV_FILE"
    direnv export bash >> "$CLAUDE_ENV_FILE" 2>/dev/null
    exit 0
fi

# Clear previous env before reloading (prevent unbounded growth)
: > "$CLAUDE_ENV_FILE"

# Fallback: parse .env files manually into CLAUDE_ENV_FILE
# Read from most specific to least specific (later values win)
for envfile in .env .env.development .env.development.local .env.local; do
    if [[ -f "$envfile" ]]; then
        while IFS= read -r line || [[ -n "$line" ]]; do
            # Skip comments and empty lines
            [[ -z "$line" || "$line" =~ ^[[:space:]]*# ]] && continue
            # Skip lines without =
            [[ "$line" == *"="* ]] || continue

            key="${line%%=*}"
            value="${line#*=}"

            # Strip leading/trailing whitespace from key
            key=$(printf '%s' "$key" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
            # Skip if key is empty or contains spaces
            [[ -z "$key" || "$key" == *" "* ]] && continue

            # Strip surrounding quotes from value
            value="${value#\"}"
            value="${value%\"}"
            value="${value#\'}"
            value="${value%\'}"

            # Escape single quotes in value to prevent injection
            escaped_value="${value//\'/\'\\\'\'}"
            printf "export %s='%s'\n" "$key" "$escaped_value" >> "$CLAUDE_ENV_FILE"
        done < "$envfile"
    fi
done

exit 0
