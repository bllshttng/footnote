#!/usr/bin/env bash
# WorktreeCreate hook: install deps, copy env, symlink .fno/, verify baseline
#
# CC fires this INSTEAD of its default git worktree behavior.
# The hook receives JSON on stdin with the worktree name and (usually) path.
# CC may or may not chdir into the worktree before invoking the hook, so we
# resolve the path from the JSON payload and cd into it ourselves (see below).
#
# Contract (Claude Code WorktreeCreate hook):
#   - stdin:  JSON with session_id, transcript_path, cwd, hook_event_name, name
#   - stdout: ONE line - the absolute worktree path. Everything else goes to
#             stderr. Exit 0 WITHOUT the path on stdout fails with
#             "WorktreeCreate hook failed: no successful output" and aborts
#             any Agent dispatch using isolation: worktree.
#   - exit:   0 on success; non-zero falls back to CC's default worktree flow.
#
# NOTE: The /speculate skill calls this script manually (not via CC hook)
# because it creates multiple worktrees in parallel via git directly.
# If this hook's behavior changes, update the copy at hooks/worktree-setup.sh
# to match - the two files are intentional duplicates for portability.
set -euo pipefail

# Read stdin JSON from CC (contains worktree name, branch, path context).
# Prefer an explicit `path` field from the harness over $(pwd) - if CC ever
# starts invoking the hook without chdir'ing, the JSON path is still right.
HOOK_INPUT=$(cat 2>/dev/null || echo "{}")
WORKTREE_PATH=""
if command -v jq >/dev/null 2>&1; then
    WORKTREE_PATH=$(printf '%s' "$HOOK_INPUT" | jq -r '.path // .worktree_path // empty' 2>/dev/null || true)
fi
[[ -z "$WORKTREE_PATH" ]] && WORKTREE_PATH="$(pwd)"
# Normalize to an absolute path and cd into it. Subsequent checks (pnpm-lock.yaml,
# node_modules, pyproject.toml, etc.) use relative paths, so they must run inside
# the worktree even if CC invoked us from a different cwd.
WORKTREE_PATH=$(cd "$WORKTREE_PATH" && pwd) || exit 1
cd "$WORKTREE_PATH" || exit 1

# Log what CC sent us (helps debug when hook behavior diverges from CC intent)
echo "WorktreeCreate input: $HOOK_INPUT" >&2
echo "WorktreeCreate resolved: path=$WORKTREE_PATH pwd=$(pwd)" >&2
MAIN_REPO=$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null | sed 's/\/.git$//')

# If we can't find the main repo, let CC handle it
[[ -n "$MAIN_REPO" ]] || exit 1

# Read config from settings.yaml if available
# Source paths.sh for typed path vars; the global tier is the per-user file, never CONFIG_FILE (ab-5d6c3d47).
if command -v fno >/dev/null 2>&1; then
    _PATHS_SH="$(fno paths shell-stub 2>/dev/null || true)"
    [[ -f "$_PATHS_SH" ]] && source "$_PATHS_SH" 2>/dev/null || true
    unset _PATHS_SH
fi
SETTINGS=""
for cfg in "$MAIN_REPO/.fno/config.toml" "${FNO_GLOBAL_SETTINGS_PATH:-$HOME/.fno/config.toml}"; do
    if [[ -f "$cfg" ]]; then
        SETTINGS="$cfg"
        break
    fi
done

# Helper: read a worktree config value from settings
wt_config() {
    local key="$1"
    local default="$2"
    if [[ -n "$SETTINGS" ]]; then
        local val
        val=$(sed -n "/^worktree:/,/^[^ ]/{ /^[[:space:]]*${key}:/{ s/.*${key}:[[:space:]]*//; s/[[:space:]]*$//; p; }; }" "$SETTINGS" 2>/dev/null | head -1)
        # Strip surrounding quotes only (preserve internal quotes)
        val="${val%\"}"; val="${val#\"}"
        val="${val%\'}"; val="${val#\'}"
        [[ -n "$val" ]] && echo "$val" || echo "$default"
    else
        echo "$default"
    fi
}

# 0. Canonical-conductor redirect (opt-in via settings.yaml).
#
# When `worktree.use_conductor_canonical: true` is set in `.fno/
# settings.yaml` (project or global), redirect the worktree from Claude
# Code's default location (`.claude/worktrees/<name>`) to
# `~/conductor/workspaces/<repo>/<name>`. Repo name comes from the
# canonical checkout's directory basename.
#
# This block expects `name` in stdin; absent (e.g. when invoked manually
# by the speculate skill which only passes `.path`), it skips the redirect
# and proceeds with the existing in-place setup path.
USE_CANONICAL="$(wt_config "use_conductor_canonical" "false")"
NAME_FROM_INPUT=""
if [[ "$USE_CANONICAL" == "true" ]] && command -v python3 >/dev/null 2>&1; then
    NAME_FROM_INPUT=$(printf '%s' "$HOOK_INPUT" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except (json.JSONDecodeError, ValueError):
    d = {}
print(d.get("name", ""))
' 2>/dev/null || true)
fi

if [[ "$USE_CANONICAL" == "true" && -n "$NAME_FROM_INPUT" ]]; then
    REPO_NAME="$(basename "$MAIN_REPO")"
    CANONICAL="$HOME/conductor/workspaces/$REPO_NAME/$NAME_FROM_INPUT"
    BRANCH_NAME="worktree-$NAME_FROM_INPUT"

    if [[ "$WORKTREE_PATH" != "$CANONICAL" ]]; then
        echo "Redirecting worktree: $WORKTREE_PATH -> $CANONICAL" >&2

        # Create the canonical worktree if it doesn't exist. Branch from
        # origin/HEAD with local-HEAD fallback. `worktree.baseRef` from
        # Claude Code settings is NOT in stdin (it's a Claude-internal
        # default), so we make our own branching decision.
        if [[ ! -d "$CANONICAL" ]]; then
            git -C "$MAIN_REPO" fetch origin >&2 2>/dev/null || true
            if git -C "$MAIN_REPO" rev-parse --verify --quiet origin/HEAD >/dev/null; then
                BASE="origin/HEAD"
            else
                BASE="HEAD"
            fi
            if git -C "$MAIN_REPO" show-ref --verify --quiet "refs/heads/$BRANCH_NAME"; then
                git -C "$MAIN_REPO" worktree add "$CANONICAL" "$BRANCH_NAME" >&2 || {
                    echo "Worktree redirect failed; leaving in place at $WORKTREE_PATH" >&2
                    CANONICAL=""
                }
            else
                git -C "$MAIN_REPO" worktree add -b "$BRANCH_NAME" "$CANONICAL" "$BASE" >&2 || {
                    echo "Worktree redirect failed; leaving in place at $WORKTREE_PATH" >&2
                    CANONICAL=""
                }
            fi
        fi

        # Best-effort: remove Claude Code's default-location worktree if
        # it was pre-created. Failures here are non-fatal - leaving an
        # empty stray under `.claude/worktrees/` beats aborting the hook
        # (non-zero exit aborts worktree creation entirely per Claude
        # Code's hook contract).
        if [[ -n "$CANONICAL" && -d "$WORKTREE_PATH" && "$WORKTREE_PATH" != "$CANONICAL" ]]; then
            git -C "$MAIN_REPO" worktree remove --force "$WORKTREE_PATH" 2>/dev/null \
                || echo "Note: could not remove pre-created worktree at $WORKTREE_PATH (non-fatal)" >&2
        fi

        if [[ -n "$CANONICAL" ]]; then
            WORKTREE_PATH="$CANONICAL"
            cd "$WORKTREE_PATH" || exit 1
        fi
    fi
fi

# 1. Copy env files from main repo
ENV_FILES=(.env .env.local .env.development .env.development.local)
for envfile in "${ENV_FILES[@]}"; do
    if [[ -f "$MAIN_REPO/$envfile" && ! -f "$WORKTREE_PATH/$envfile" ]]; then
        cp "$MAIN_REPO/$envfile" "$WORKTREE_PATH/$envfile" || true
        echo "Copied $envfile from main repo" >&2
    fi
done

# 2. Symlink .fno/ from main repo (shared state)
if [[ -d "$MAIN_REPO/.fno" && ! -L "$WORKTREE_PATH/.fno" && ! -e "$WORKTREE_PATH/.fno" ]]; then
    ln -s "$MAIN_REPO/.fno" "$WORKTREE_PATH/.fno" || true
    echo "Symlinked .fno/" >&2
fi

# 3. Auto-detect and install deps (skip if already present)
# Set worktree.auto_install: false in .fno/settings.yaml to skip dep
# installation entirely. Useful when target creates many worktrees of the same
# project — each fresh .venv otherwise materializes its own resolved deps in
# the uv cache (45GB+ bloat at scale).
AUTO_INSTALL=$(wt_config "auto_install" "true")
SETUP_CMD=$(wt_config "setup_command" "")
if [[ -n "$SETUP_CMD" ]]; then
    echo "Running custom setup: $SETUP_CMD" >&2
    bash -c "$SETUP_CMD" 2>&1 | tail -5 >&2
elif [[ "$AUTO_INSTALL" == "false" ]]; then
    echo "Skipping dep install (worktree.auto_install: false)" >&2
elif [[ -f "pnpm-lock.yaml" && ! -d "node_modules" ]]; then
    pnpm install --frozen-lockfile 2>&1 | tail -3 >&2
elif [[ -f "package-lock.json" && ! -d "node_modules" ]]; then
    npm ci 2>&1 | tail -3 >&2
elif [[ -f "yarn.lock" && ! -d "node_modules" ]]; then
    yarn install --frozen-lockfile 2>&1 | tail -3 >&2
elif [[ -f "bun.lockb" && ! -d "node_modules" ]]; then
    bun install 2>&1 | tail -3 >&2
elif [[ -f "requirements.txt" && ! -d ".venv" ]]; then
    python3 -m venv .venv >&2 2>&1
    .venv/bin/pip install -r requirements.txt 2>&1 | tail -3 >&2
elif [[ -f "pyproject.toml" && ! -d ".venv" ]]; then
    if command -v uv >/dev/null 2>&1; then
        uv sync 2>&1 | tail -3 >&2
    else
        python3 -m venv .venv >&2 2>&1
        .venv/bin/pip install . 2>&1 | tail -3 >&2
    fi
fi

# 4. Run quick verification (non-blocking)
SKIP_VERIFY=$(wt_config "skip_verification" "false")
if [[ "$SKIP_VERIFY" != "true" ]]; then
    TEST_CMD=$(wt_config "test_command" "")
    if [[ -n "$TEST_CMD" ]]; then
        echo "Running baseline verification: $TEST_CMD" >&2
        bash -c "$TEST_CMD" 2>&1 | tail -5 >&2 || echo "Warning: baseline verification failed (non-blocking)" >&2
    fi
fi

# 5. Log lifecycle event
TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
echo "{\"ts\":\"$TS\",\"action\":\"created\",\"path\":\"$WORKTREE_PATH\"}" >> "$MAIN_REPO/.fno/worktree-log.jsonl" 2>/dev/null

echo "Worktree ready: $WORKTREE_PATH" >&2

# CC contract: emit the absolute worktree path on stdout as the sole success
# signal. Everything else in this hook logs to stderr so stdout stays clean.
echo "$WORKTREE_PATH"
exit 0
