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
# If this hook's behavior changes, update the copy at
# skills/speculate/scripts/worktree-setup.sh to match - the two files are
# intentional duplicates for portability.
#
# DIVERGENCE (worktrees_base migration, x-33e9): the relocation logic in
# block 0 below (honor config.paths.worktrees_base, else leave harness-native)
# is intentionally HOOK-ONLY and must NOT be copied to the /speculate
# duplicate. /speculate deliberately materializes its parallel variations at
# .claude/worktrees/<name> (a sanctioned exception, like the cross-project
# pipeline); relocating those to a configured base would break it. The rest of
# the two files stay in sync.
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

# Read config from settings.yaml if available.
# Source paths.sh for typed path vars; the global tier is the per-user file, never CONFIG_FILE (ab-5d6c3d47).
if command -v fno >/dev/null 2>&1; then
    PATHS_SH="$(fno paths shell-stub 2>/dev/null || true)"
    [[ -f "$PATHS_SH" ]] && source "$PATHS_SH" 2>/dev/null || true
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
    local val=""
    if [[ -n "$SETTINGS" ]] && command -v yq >/dev/null 2>&1; then
        # Flat config.toml: worktree keys live under the [worktree] table.
        val=$(yq -p toml -r ".worktree.${key} // \"\"" "$SETTINGS" 2>/dev/null)
    fi
    if [[ -n "$val" && "$val" != "null" ]]; then
        echo "$val"
    else
        echo "$default"
    fi
}

# 0. Worktree relocation: honor config.paths.worktrees_base (OSS-neutral).
#
# Resolution order (x-33e9, worktrees_base migration):
#   1. config.paths.worktrees_base set -> relocate to <base>/<repo>/<name>.
#   2. else worktree.use_conductor_canonical: true (DEPRECATED back-compat)
#      -> relocate to ~/conductor/workspaces/<repo>/<name>.
#   3. else (unset) -> harness-native: leave the worktree where Claude Code
#      placed it (`<repo>/.claude/worktrees/<name>`). No relocation. That dir
#      is gitignored, so rg/Grep already skip it - the old "inside-checkout is
#      always forbidden" redirect is retired; harness-native is now the default.
#
# `worktrees_base` is read as the RAW config field (empty when unset) via
# `fno config get`. The paths.sh `WORKTREES_BASE` var always carries the
# ~/.fno/worktrees default, so it cannot distinguish "unset" from "set to the
# default" - the distinction that decides relocate-vs-leave-in-place.
WT_BASE_RAW=""
if command -v fno >/dev/null 2>&1; then
    WT_BASE_RAW="$(fno config get config.paths.worktrees_base 2>/dev/null || true)"
fi
[[ "$WT_BASE_RAW" == "null" ]] && WT_BASE_RAW=""
USE_CANONICAL="$(wt_config "use_conductor_canonical" "false")"

RELOCATE_BASE=""
if [[ -n "$WT_BASE_RAW" ]]; then
    # Config stores ~ literally; expand a leading ~ to $HOME.
    RELOCATE_BASE="${WT_BASE_RAW/#\~/$HOME}"
elif [[ "$USE_CANONICAL" == "true" ]]; then
    echo "Note: worktree.use_conductor_canonical is DEPRECATED; set config.paths.worktrees_base: ~/conductor/workspaces instead." >&2
    RELOCATE_BASE="$HOME/conductor/workspaces"
fi

# Worktree name (from stdin; fall back to the path basename, e.g. when the
# /speculate skill invokes this manually with only `.path`).
NAME_FROM_INPUT=""
if command -v python3 >/dev/null 2>&1; then
    NAME_FROM_INPUT=$(printf '%s' "$HOOK_INPUT" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except (json.JSONDecodeError, ValueError):
    d = {}
print(d.get("name", ""))
' 2>/dev/null || true)
fi
[[ -z "$NAME_FROM_INPUT" ]] && NAME_FROM_INPUT="$(basename "$WORKTREE_PATH")"

if [[ -n "$RELOCATE_BASE" && -n "$NAME_FROM_INPUT" ]]; then
    REPO_NAME="$(basename "$MAIN_REPO")"
    CANONICAL="$RELOCATE_BASE/$REPO_NAME/$NAME_FROM_INPUT"
    BRANCH_NAME="worktree-$NAME_FROM_INPUT"

    if [[ "$WORKTREE_PATH" != "$CANONICAL" ]]; then
        echo "Relocating worktree: $WORKTREE_PATH -> $CANONICAL" >&2

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
                # The branch already exists. If it is checked out by the
                # worktree we are redirecting away (CC pre-created
                # .claude/worktrees/<name> on `worktree-<name>`), `git worktree
                # add` would fail with "branch already checked out" and the
                # redirect would silently leave the forbidden nested worktree in
                # place. Free the branch by removing that worktree first - it is
                # being redirected away regardless.
                if [[ -d "$WORKTREE_PATH" && "$WORKTREE_PATH" != "$CANONICAL" ]]; then
                    git -C "$MAIN_REPO" worktree remove --force "$WORKTREE_PATH" 2>/dev/null || true
                fi
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
