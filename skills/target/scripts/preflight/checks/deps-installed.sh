#!/usr/bin/env bash
# deps-installed.sh - Check if project dependencies are installed
# Contract: stdout one line "deps-installed {pass|fail|warn|unknown} {message}"
# Exit: always 0
# Note: missing tooling is WARN (informational), not FAIL

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"

CHECKS_DONE=0
WARNINGS=()

# --- Node.js / npm / pnpm / yarn checks ---
if [[ -f "$REPO_ROOT/pnpm-lock.yaml" ]]; then
    CHECKS_DONE=$((CHECKS_DONE + 1))
    if ! command -v pnpm >/dev/null 2>&1; then
        WARNINGS+=("pnpm not in PATH")
    elif [[ ! -d "$REPO_ROOT/node_modules" ]]; then
        WARNINGS+=("node_modules missing (run: pnpm install)")
    else
        # Check if lockfile is newer than node_modules (deps out of date)
        if [[ "$REPO_ROOT/pnpm-lock.yaml" -nt "$REPO_ROOT/node_modules" ]]; then
            WARNINGS+=("pnpm-lock.yaml newer than node_modules (run: pnpm install)")
        fi
    fi
elif [[ -f "$REPO_ROOT/package-lock.json" ]]; then
    CHECKS_DONE=$((CHECKS_DONE + 1))
    if ! command -v npm >/dev/null 2>&1; then
        WARNINGS+=("npm not in PATH")
    elif [[ ! -d "$REPO_ROOT/node_modules" ]]; then
        WARNINGS+=("node_modules missing (run: npm install)")
    fi
elif [[ -f "$REPO_ROOT/yarn.lock" ]]; then
    CHECKS_DONE=$((CHECKS_DONE + 1))
    if ! command -v yarn >/dev/null 2>&1; then
        WARNINGS+=("yarn not in PATH")
    elif [[ ! -d "$REPO_ROOT/node_modules" ]]; then
        WARNINGS+=("node_modules missing (run: yarn install)")
    fi
fi

# --- Python / pip / uv checks ---
if [[ -f "$REPO_ROOT/pyproject.toml" || -f "$REPO_ROOT/requirements.txt" || -f "$REPO_ROOT/uv.lock" ]]; then
    CHECKS_DONE=$((CHECKS_DONE + 1))
    if [[ -f "$REPO_ROOT/uv.lock" ]]; then
        if ! command -v uv >/dev/null 2>&1; then
            WARNINGS+=("uv not in PATH")
        elif [[ ! -d "$REPO_ROOT/.venv" ]]; then
            WARNINGS+=(".venv missing (run: uv sync)")
        fi
    elif [[ -f "$REPO_ROOT/requirements.txt" ]]; then
        if ! command -v pip >/dev/null 2>&1 && ! command -v pip3 >/dev/null 2>&1; then
            WARNINGS+=("pip not in PATH")
        fi
    fi
fi

if [[ $CHECKS_DONE -eq 0 ]]; then
    echo "deps-installed unknown no recognized lockfile found (not a Node or Python project)"
    exit 0
fi

if [[ ${#WARNINGS[@]} -eq 0 ]]; then
    echo "deps-installed pass all dependencies appear up to date"
    exit 0
fi

WARN_MSG=$(printf '%s; ' "${WARNINGS[@]}")
WARN_MSG="${WARN_MSG%; }"
echo "deps-installed warn $WARN_MSG"
exit 0
