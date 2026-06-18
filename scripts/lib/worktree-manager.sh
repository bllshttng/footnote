#!/usr/bin/env bash
# scripts/lib/worktree-manager.sh
#
# Unified worktree management for target / megawalk
# and the cross-project pipeline. One source of truth for path conventions,
# branch naming, setup caching, env-file copying, and cleanup semantics.
#
# Verbs:
#   create   <project> <slug> [--mode=manual|ephemeral] [--branch=<name>]
#   setup    <worktree-path> [--force]
#   cleanup  [--mode=ephemeral|stale|all] [--older-than=Nd] [--dry-run] [--prefix=<prefix>]
#   migrate  [--auto] [--dry-run]
#   resolve  <project>           # echoes worktree_base for <project>
#
# JSON contract:
#   create / setup / migrate emit JSON on stdout. All other output goes to stderr.
#   Status codes: 0 = success, 1 = error, 2 = idempotent no-op.
#
# Settings lookup chain (project-local wins over global):
#   1. <repo>/.fno/settings.yaml
#   2. ~/.fno/settings.yaml
#
# Path resolution chain for create:
#   1. settings.yaml worktree_base for the named project
#   2. <project_path>/.claude/worktrees   (back-compat default)
#
# Branch naming:
#   manual mode    -> feature/{slug}              (predictable, retrievable)
#   ephemeral mode -> caller-supplied via --branch (defaults to {slug})
#
# Setup caching:
#   First call to setup runs the project's setup_command (or auto-detects
#   from package manager) and writes a lockfile-hash to
#   <worktree>/.fno/setup-cache.txt. Subsequent calls compare the
#   current lockfile hash against the cache and skip install on match.

set -uo pipefail

# ----------------------------------------------------------------------
# Common helpers
# ----------------------------------------------------------------------

WTM_VERSION="1"
WTM_LOCAL_SETTINGS="${WTM_LOCAL_SETTINGS:-.fno/settings.yaml}"
WTM_GLOBAL_SETTINGS="${WTM_GLOBAL_SETTINGS:-$HOME/.fno/settings.yaml}"

# Resolve the directory holding this script. Used to locate sibling helpers
# (e.g. scripts/lib/worktree-lifecycle.sh) regardless of the caller's cwd.
# scripts/lib/ -> scripts/ -> repo root.
_WTM_SELF="${BASH_SOURCE[0]:-$0}"
WTM_SCRIPT_DIR="$(cd "$(dirname "$_WTM_SELF")" && pwd)"
WTM_PLUGIN_ROOT="$(cd "$WTM_SCRIPT_DIR/../.." && pwd)"

# Memoize the calling repo root. `setup` is a hot path for every cross-
# project worker and every target worktree creation, and the verbs each call
# `git rev-parse` for the same answer 2-3 times. One subprocess per script
# invocation is enough.
_WTM_REPO_ROOT_CACHED=""

_wtm_log() { echo "wtm: $*" >&2; }
_wtm_err() { echo "wtm: ERROR: $*" >&2; }

# Print a JSON object built from key=value pairs. Strings only - we keep this
# pure-bash so we don't take a hard dep on jq.
# Usage: _wtm_json status=ok path="$P" branch="$B"
_wtm_json() {
    local first=1
    printf '{'
    while [[ $# -gt 0 ]]; do
        local kv="$1"; shift
        local key="${kv%%=*}"
        local val="${kv#*=}"
        # Escape backslashes, then double quotes
        val="${val//\\/\\\\}"
        val="${val//\"/\\\"}"
        if [[ $first -eq 1 ]]; then
            first=0
        else
            printf ', '
        fi
        # Booleans and numbers pass through; everything else is quoted.
        if [[ "$val" =~ ^(true|false|null)$ ]] || [[ "$val" =~ ^-?[0-9]+(\.[0-9]+)?$ ]]; then
            printf '"%s": %s' "$key" "$val"
        else
            printf '"%s": "%s"' "$key" "$val"
        fi
    done
    printf '}\n'
}

# Expand ~ to $HOME (single leading tilde only - mirrors the same fix shipped
# in the graph adopt detector for path comparisons). Use case + parameter
# substring extraction to avoid ambiguity with bash's [[ pattern matching,
# which treats unquoted ~ specially.
_wtm_expand_tilde() {
    local p="$1"
    case "$p" in
        '~')    echo "$HOME" ;;
        '~/'*)  echo "$HOME/${p:2}" ;;
        *)      echo "$p" ;;
    esac
}

_wtm_repo_root() {
    if [[ -z "$_WTM_REPO_ROOT_CACHED" ]]; then
        _WTM_REPO_ROOT_CACHED=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
    fi
    echo "$_WTM_REPO_ROOT_CACHED"
}

# Echo all candidate settings.yaml files in lookup order (local then global),
# one path per line. Callers iterate and short-circuit on first match. This
# mirrors get_config: project-local provides overrides, global provides
# defaults. A project that isn't declared locally still gets its global
# `worktree_base` honored.
_wtm_settings_files() {
    local local_path="$(_wtm_repo_root)/$WTM_LOCAL_SETTINGS"
    [[ -f "$local_path" ]] && echo "$local_path"
    [[ -f "$WTM_GLOBAL_SETTINGS" && "$local_path" != "$WTM_GLOBAL_SETTINGS" ]] && echo "$WTM_GLOBAL_SETTINGS"
}

# Walk both settings files looking for `work.workspaces[].projects[].name == $project`
# (canonical multi-workspace shape) and `work.projects[].name == $project` (legacy
# flat shape). Echo the first non-null match for the requested field. Used by
# _wtm_resolve_base (worktree_base) and _wtm_resolve_project_path (path).
_wtm_yq_project_field() {
    local project="$1" field="$2"
    command -v yq >/dev/null 2>&1 || return 1
    # Pass project name and field through environment (env(...)) instead of
    # string interpolation. A project name containing `"` or `\` would
    # otherwise break the yq query and silently fall back to defaults.
    local raw="" settings
    while IFS= read -r settings; do
        [[ -n "$settings" ]] || continue
        raw=$(_wtm_p="$project" _wtm_f="$field" yq -r \
              '.work.workspaces[].projects[]? | select(.name == env(_wtm_p)) | .[env(_wtm_f)] // ""' \
              "$settings" 2>/dev/null | head -1)
        if [[ -z "$raw" || "$raw" == "null" ]]; then
            raw=$(_wtm_p="$project" _wtm_f="$field" yq -r \
                  '.work.projects[]? | select(.name == env(_wtm_p)) | .[env(_wtm_f)] // ""' \
                  "$settings" 2>/dev/null | head -1)
        fi
        if [[ -n "$raw" && "$raw" != "null" ]]; then
            echo "$raw"
            return 0
        fi
    done < <(_wtm_settings_files)
    return 1
}

# Resolve the project's worktree_base. Echoes the absolute base directory
# (without the slug component) and returns 0. Falls back to
# <project_path>/.claude/worktrees if the project has no worktree_base set
# OR if no settings.yaml exists at all (back-compat default).
#
# When called from inside the project repo, the special name "." means
# "use the current repo's name from settings.yaml or fall back to the dir
# basename".
_wtm_resolve_base() {
    local project="$1"
    local repo_root
    repo_root=$(_wtm_repo_root)

    if [[ "$project" == "." || -z "$project" ]]; then
        project=$(basename "$repo_root")
    fi

    local raw
    if raw=$(_wtm_yq_project_field "$project" worktree_base); then
        _wtm_expand_tilde "$raw"
        return 0
    fi
    # Back-compat default: per-repo .claude/worktrees
    echo "$repo_root/.claude/worktrees"
}

# Resolve a project's `path:` field from settings.yaml. Used by setup to
# locate the source repo for env-file copying. Echoes empty if not found.
_wtm_resolve_project_path() {
    local raw
    if raw=$(_wtm_yq_project_field "$1" path); then
        _wtm_expand_tilde "$raw"
        return 0
    fi
    return 1
}

# Compute a deterministic hash of the project's primary lockfile.
# Echoes "<algo>:<hex>". Returns 1 if no lockfile is found.
_wtm_lockfile_hash() {
    local dir="$1"
    local f
    for f in pnpm-lock.yaml package-lock.json yarn.lock bun.lockb uv.lock requirements.txt poetry.lock Cargo.lock; do
        if [[ -f "$dir/$f" ]]; then
            local hex
            if command -v sha256sum >/dev/null 2>&1; then
                hex=$(sha256sum "$dir/$f" | awk '{print $1}')
            else
                hex=$(shasum -a 256 "$dir/$f" | awk '{print $1}')
            fi
            echo "$f:$hex"
            return 0
        fi
    done
    return 1
}

# Auto-detect the project's setup command. Echoes the command string.
_wtm_detect_setup_cmd() {
    local dir="$1"
    if [[ -f "$dir/pnpm-lock.yaml" ]]; then echo "pnpm install --frozen-lockfile"
    elif [[ -f "$dir/package-lock.json" ]]; then echo "npm ci"
    elif [[ -f "$dir/yarn.lock" ]]; then echo "yarn install --frozen-lockfile"
    elif [[ -f "$dir/bun.lockb" ]]; then echo "bun install"
    elif [[ -f "$dir/uv.lock" || -f "$dir/pyproject.toml" ]]; then
        if command -v uv >/dev/null 2>&1; then echo "uv sync"
        else echo "python3 -m venv .venv && .venv/bin/pip install -e ."
        fi
    elif [[ -f "$dir/requirements.txt" ]]; then
        echo "python3 -m venv .venv && .venv/bin/pip install -r requirements.txt"
    elif [[ -f "$dir/Cargo.toml" ]]; then echo "cargo build"
    else
        echo ""
    fi
}

# Read worktree config from settings.yaml. Returns a default if unset.
# Usage: _wtm_config_value <key> <default>
_wtm_config_value() {
    local key="$1" default="$2"
    local settings
    settings=$(_wtm_settings_files | head -1)
    if [[ -z "$settings" ]] || ! command -v yq >/dev/null 2>&1; then
        echo "$default"
        return 0
    fi
    local val
    val=$(yq -r ".config.worktree.$key // \"\"" "$settings" 2>/dev/null)
    if [[ -z "$val" || "$val" == "null" ]]; then
        echo "$default"
    else
        echo "$val"
    fi
}

# Read worktree.env_files array from settings.yaml. Echoes one filename per line.
_wtm_env_files() {
    local settings
    settings=$(_wtm_settings_files | head -1)
    if [[ -z "$settings" ]] || ! command -v yq >/dev/null 2>&1; then
        printf '.env\n.env.local\n.env.development\n.env.development.local\n'
        return 0
    fi
    local lines
    lines=$(yq -r '.config.worktree.env_files[]?' "$settings" 2>/dev/null)
    if [[ -z "$lines" ]]; then
        printf '.env\n.env.local\n.env.development\n.env.development.local\n'
    else
        echo "$lines"
    fi
}

# ----------------------------------------------------------------------
# Verb: resolve
# ----------------------------------------------------------------------
_wtm_cmd_resolve() {
    local project="${1:-.}"
    _wtm_resolve_base "$project"
}

# ----------------------------------------------------------------------
# Verb: create
# ----------------------------------------------------------------------
_wtm_cmd_create() {
    local project="" slug=""
    local mode="manual"
    local branch=""

    # Positional: project, slug. Then flags.
    if [[ $# -ge 1 && "${1:0:2}" != "--" ]]; then project="$1"; shift; fi
    if [[ $# -ge 1 && "${1:0:2}" != "--" ]]; then slug="$1"; shift; fi
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --mode=*)   mode="${1#--mode=}"; shift ;;
            --branch=*) branch="${1#--branch=}"; shift ;;
            --mode)     mode="$2"; shift 2 ;;
            --branch)   branch="$2"; shift 2 ;;
            *)          _wtm_err "unknown flag: $1"; return 1 ;;
        esac
    done

    if [[ -z "$project" || -z "$slug" ]]; then
        _wtm_err "create requires <project> <slug>"
        return 1
    fi

    case "$mode" in
        manual|ephemeral) ;;
        *) _wtm_err "invalid --mode=$mode (expected manual|ephemeral)"; return 1 ;;
    esac

    if [[ -z "$branch" ]]; then
        if [[ "$mode" == "manual" ]]; then
            branch="feature/${slug}"
        else
            branch="${slug}"
        fi
    fi

    local base
    base=$(_wtm_resolve_base "$project")
    local worktree_path="$base/$slug"

    # Idempotent: existing worktree at the target path is a hit.
    if [[ -d "$worktree_path" && -e "$worktree_path/.git" ]]; then
        local cur_branch
        cur_branch=$(git -C "$worktree_path" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "?")
        _wtm_log "worktree exists at $worktree_path (branch: $cur_branch)"
        _wtm_json status=ok existing=true path="$worktree_path" branch="$cur_branch" mode="$mode"
        return 0
    fi

    mkdir -p "$base"

    # Try to create with a new branch; if the branch already exists, attach to it.
    # IMPORTANT: redirect git's stdout to stderr - `git worktree add` prints
    # "HEAD is now at..." to stdout, which would otherwise pollute the JSON
    # we emit at the end of this verb.
    local err_log
    err_log=$(mktemp) || { _wtm_err "failed to create temp file"; _wtm_json status=error error="mktemp failed"; return 1; }
    if git worktree add "$worktree_path" -b "$branch" >&2 2>"$err_log"; then
        _wtm_log "created worktree $worktree_path on new branch $branch"
    else
        _wtm_log "branch $branch may already exist, attaching ($(cat "$err_log"))"
        if ! git worktree add "$worktree_path" "$branch" >&2 2>>"$err_log"; then
            _wtm_err "git worktree add failed: $(cat "$err_log")"
            rm -f "$err_log"
            _wtm_json status=error error="git worktree add failed"
            return 1
        fi
    fi
    rm -f "$err_log"

    _wtm_json status=ok existing=false path="$worktree_path" branch="$branch" mode="$mode"
}

# ----------------------------------------------------------------------
# Verb: setup
# ----------------------------------------------------------------------
_wtm_cmd_setup() {
    local worktree_path=""
    local force=0

    if [[ $# -ge 1 && "${1:0:2}" != "--" ]]; then worktree_path="$1"; shift; fi
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --force) force=1; shift ;;
            *) _wtm_err "unknown flag: $1"; return 1 ;;
        esac
    done

    if [[ -z "$worktree_path" ]]; then
        _wtm_err "setup requires <worktree-path>"
        return 1
    fi
    worktree_path=$(_wtm_expand_tilde "$worktree_path")
    if [[ ! -d "$worktree_path" ]]; then
        _wtm_err "worktree path does not exist: $worktree_path"
        return 1
    fi

    # Locate the main repo to source env files from. We use the worktree's
    # own git common dir (which points at the canonical repo) - this gives us
    # the source of truth even if the cwd isn't the main repo.
    #
    # --path-format=absolute requires git >= 2.31 (released 2021). Some LTS
    # systems (Ubuntu 20.04, RHEL 8 base) ship older git, where the flag is
    # rejected and rev-parse silently fails. Drop the flag and resolve the
    # absolute path manually if the value comes back relative.
    local common_dir
    common_dir=$(git -C "$worktree_path" rev-parse --git-common-dir 2>/dev/null || echo "")
    local main_repo=""
    if [[ -n "$common_dir" ]]; then
        if [[ "$common_dir" != /* ]]; then
            common_dir=$(cd "$worktree_path" && cd "$common_dir" 2>/dev/null && pwd) || common_dir=""
        fi
    fi
    if [[ -n "$common_dir" ]]; then
        if [[ "$common_dir" == */.git || "$common_dir" == ".git" ]]; then
            main_repo=$(dirname "$common_dir")
        elif [[ "$common_dir" == *.git/worktrees/* ]]; then
            # Linked worktree: .git/worktrees/<name>; back up to the canonical .git
            main_repo=$(dirname "$(dirname "$(dirname "$common_dir")")")
        else
            main_repo="$common_dir"
        fi
    fi
    if [[ -z "$main_repo" || ! -d "$main_repo" ]]; then
        _wtm_log "could not resolve main repo for $worktree_path - skipping env copy"
        main_repo=""
    fi

    # Copy env files (only those declared in settings; only if not present).
    # Create the destination directory first so subdirectory env paths
    # (e.g. `config/.env`) work even when the worktree doesn't yet have
    # the parent dir.
    local copied=0
    if [[ -n "$main_repo" ]]; then
        while IFS= read -r envfile; do
            [[ -z "$envfile" ]] && continue
            if [[ -f "$main_repo/$envfile" && ! -e "$worktree_path/$envfile" ]]; then
                mkdir -p "$(dirname "$worktree_path/$envfile")" 2>/dev/null
                cp "$main_repo/$envfile" "$worktree_path/$envfile" 2>/dev/null \
                    && copied=$((copied + 1)) \
                    && _wtm_log "copied $envfile from main repo"
            fi
        done < <(_wtm_env_files)
    fi

    mkdir -p "$worktree_path/.fno"
    local cache_file="$worktree_path/.fno/setup-cache.txt"

    # Compare lockfile hash. Skip install on match (unless --force).
    local current_hash old_hash="" cached=false
    if current_hash=$(_wtm_lockfile_hash "$worktree_path"); then
        if [[ -f "$cache_file" ]]; then
            old_hash=$(cat "$cache_file" 2>/dev/null || echo "")
        fi
        if [[ "$force" -eq 0 && "$current_hash" == "$old_hash" ]]; then
            cached=true
            _wtm_log "setup cache hit ($current_hash) - skipping install"
            _wtm_json status=ok cached=true env_files_copied="$copied" \
                lockfile_hash="$current_hash"
            return 0
        fi
    fi

    # Run the configured setup command, or auto-detect.
    local setup_cmd
    setup_cmd=$(_wtm_config_value setup_command "")
    if [[ -z "$setup_cmd" ]]; then
        setup_cmd=$(_wtm_detect_setup_cmd "$worktree_path")
    fi

    local install_status="skipped"
    if [[ -n "$setup_cmd" ]]; then
        _wtm_log "running setup: $setup_cmd"
        if (cd "$worktree_path" && bash -c "$setup_cmd") >&2; then
            install_status="ok"
            if [[ -n "$current_hash" ]]; then
                echo "$current_hash" > "$cache_file"
            fi
        else
            install_status="failed"
            _wtm_err "setup command failed: $setup_cmd"
            _wtm_json status=error cached=false env_files_copied="$copied" \
                install="$install_status"
            return 1
        fi
    fi

    _wtm_json status=ok cached=false env_files_copied="$copied" \
        install="$install_status" lockfile_hash="${current_hash:-none}"
}

# ----------------------------------------------------------------------
# Verb: cleanup
# ----------------------------------------------------------------------
# Wraps scripts/lib/worktree-lifecycle.sh with the manager's path
# conventions and a mode selector.
_wtm_cmd_cleanup() {
    local mode="all"
    local older_than="7"
    local dry_run=""
    local prefix=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --mode=*)        mode="${1#--mode=}"; shift ;;
            --mode)          mode="$2"; shift 2 ;;
            --older-than=*)  older_than="${1#--older-than=}"; older_than="${older_than%d}"; shift ;;
            --older-than)    older_than="${2%d}"; shift 2 ;;
            --dry-run)       dry_run="--dry-run"; shift ;;
            --prefix=*)      prefix="${1#--prefix=}"; shift ;;
            --prefix)        prefix="$2"; shift 2 ;;
            *)               _wtm_err "unknown flag: $1"; return 1 ;;
        esac
    done

    case "$mode" in
        ephemeral|stale|all) ;;
        *) _wtm_err "invalid --mode=$mode (expected ephemeral|stale|all)"; return 1 ;;
    esac

    # Delegate to the canonical lifecycle script. The mode flag adjusts the
    # prefix filter:
    #   - ephemeral -> only worktrees whose branch starts with "discover/" or "speculate/"
    #   - stale     -> exclude ephemeral prefixes (so this complements --mode=ephemeral)
    #   - all       -> no extra filter (default behavior of lifecycle script)
    #
    # Resolve the lifecycle script relative to THIS script (WTM_PLUGIN_ROOT),
    # not the caller's cwd. The cleanup verb runs git operations in the
    # caller's repo, but the script itself ships alongside this manager.
    local lifecycle_script="$WTM_PLUGIN_ROOT/scripts/lib/worktree-lifecycle.sh"
    if [[ ! -f "$lifecycle_script" ]]; then
        _wtm_err "lifecycle script not found: $lifecycle_script"
        return 1
    fi

    local args=(cleanup --older-than "$older_than")
    [[ -n "$dry_run" ]] && args+=("$dry_run")
    if [[ -n "$prefix" ]]; then
        args+=(--prefix "$prefix")
    elif [[ "$mode" == "ephemeral" ]]; then
        # Two passes for the two ephemeral prefixes - the lifecycle script
        # supports a single --prefix at a time.
        bash "$lifecycle_script" cleanup --older-than "$older_than" \
            ${dry_run:+$dry_run} --prefix discover/ >&2
        bash "$lifecycle_script" cleanup --older-than "$older_than" \
            ${dry_run:+$dry_run} --prefix speculate/ >&2
        return 0
    elif [[ "$mode" == "stale" ]]; then
        # Stale = everything that ISN'T ephemeral. The lifecycle script
        # supports only positive prefix matching, so we filter the worktree
        # list ourselves and pass each non-ephemeral path as its own prefix.
        # In practice this means feature/* branches; matching by branch name
        # rather than path is more reliable across worktree_base layouts.
        local stale_branches
        stale_branches=$(git -C "$(_wtm_repo_root)" worktree list --porcelain 2>/dev/null \
            | awk '/^branch /{print substr($2, 12)}' \
            | grep -vE '^(discover/|speculate/)' \
            | grep -vE '^(main|master)$' || true)
        if [[ -z "$stale_branches" ]]; then
            _wtm_log "no non-ephemeral worktrees match stale criteria"
            return 0
        fi
        while IFS= read -r br; do
            [[ -z "$br" ]] && continue
            bash "$lifecycle_script" cleanup --older-than "$older_than" \
                ${dry_run:+$dry_run} --prefix "$br" >&2
        done <<< "$stale_branches"
        return 0
    fi

    bash "$lifecycle_script" "${args[@]}" >&2
}

# ----------------------------------------------------------------------
# Verb: migrate
# ----------------------------------------------------------------------
# One-shot: scan known worktree locations, classify each as STALE or LIVE,
# and (with --auto) remove the stale ones. Live = target-state.md status is
# IN_PROGRESS. Without --auto, prints a JSON manifest and exits.
_wtm_cmd_migrate() {
    local auto=0
    local dry_run=0

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --auto)    auto=1; shift ;;
            --dry-run) dry_run=1; shift ;;
            *) _wtm_err "unknown flag: $1"; return 1 ;;
        esac
    done

    local repo_root
    repo_root=$(_wtm_repo_root)

    # Candidate locations. We don't try to recurse - just list the known
    # base dirs.
    local candidates=()
    if [[ -d "$repo_root/.claude/worktrees" ]]; then
        candidates+=("$repo_root/.claude/worktrees")
    fi
    if [[ -d "$HOME/conductor/workspaces" ]]; then
        candidates+=("$HOME/conductor/workspaces")
    fi

    if [[ ${#candidates[@]} -eq 0 ]]; then
        _wtm_json status=ok inspected=0 stale=0 live=0
        return 0
    fi

    local -a stale=() live=()
    local base wt
    for base in "${candidates[@]}"; do
        for wt in "$base"/*/; do
            [[ -d "$wt" ]] || continue
            wt="${wt%/}"
            # Heuristic: must be a worktree (have a .git file or symlinked .git)
            if [[ ! -e "$wt/.git" ]]; then
                # Could be a workspace root that contains worktrees; descend one level
                local sub
                for sub in "$wt"/*/; do
                    [[ -d "$sub" ]] || continue
                    sub="${sub%/}"
                    [[ -e "$sub/.git" ]] || continue
                    if _wtm_is_live "$sub"; then live+=("$sub"); else stale+=("$sub"); fi
                done
                continue
            fi
            if _wtm_is_live "$wt"; then live+=("$wt"); else stale+=("$wt"); fi
        done
    done

    # Bash 3.2 + set -u tripwire: ${arr[@]} on an empty array errors out.
    # Substring-default ("${arr[@]:-}") sidesteps it for read access; we also
    # gate the for-loops on count so we don't iterate over a literal empty
    # default token.
    local stale_count=${#stale[@]}
    local live_count=${#live[@]}

    if [[ "$auto" -eq 1 && "$dry_run" -eq 0 ]]; then
        local removed=0
        if [[ $stale_count -gt 0 ]]; then
            for wt in "${stale[@]}"; do
                _wtm_log "removing stale worktree: $wt"
                git -C "$repo_root" worktree remove --force "$wt" 2>/dev/null \
                    && removed=$((removed + 1)) \
                    || _wtm_log "failed to remove $wt (try git worktree prune)"
            done
        fi
        _wtm_json status=ok inspected=$(( stale_count + live_count )) \
            stale="$stale_count" live="$live_count" removed="$removed"
    else
        _wtm_log "Stale worktrees ($stale_count):"
        if [[ $stale_count -gt 0 ]]; then
            for wt in "${stale[@]}"; do _wtm_log "  $wt"; done
        fi
        _wtm_log "Live worktrees ($live_count):"
        if [[ $live_count -gt 0 ]]; then
            for wt in "${live[@]}"; do _wtm_log "  $wt"; done
        fi
        _wtm_json status=ok inspected=$(( stale_count + live_count )) \
            stale="$stale_count" live="$live_count" removed=0
    fi
}

_wtm_is_live() {
    local wt="$1"
    local state="$wt/.fno/target-state.md"
    [[ -f "$state" ]] || return 1
    # Fail-safe: an unreadable target-state.md is treated as LIVE so migrate
    # --auto never force-removes a worktree we couldn't classify. The only
    # path to deletion is "we read the state and it explicitly said not
    # IN_PROGRESS" - never "we couldn't tell".
    [[ -r "$state" ]] || return 0
    local status
    status=$(grep -E "^[[:space:]]*status:" "$state" 2>/dev/null | head -1 | awk '{print $2}')
    [[ "$status" == "IN_PROGRESS" ]]
}

# ----------------------------------------------------------------------
# Dispatcher
# ----------------------------------------------------------------------
main() {
    local verb="${1:-}"
    shift || true
    case "$verb" in
        create)  _wtm_cmd_create  "$@" ;;
        setup)   _wtm_cmd_setup   "$@" ;;
        cleanup) _wtm_cmd_cleanup "$@" ;;
        migrate) _wtm_cmd_migrate "$@" ;;
        resolve) _wtm_cmd_resolve "$@" ;;
        version) echo "$WTM_VERSION" ;;
        ""|-h|--help|help)
            cat <<'USAGE' >&2
Usage: worktree-manager.sh <verb> [args]

Verbs:
  create   <project> <slug> [--mode=manual|ephemeral] [--branch=<name>]
  setup    <worktree-path> [--force]
  cleanup  [--mode=ephemeral|stale|all] [--older-than=Nd] [--dry-run] [--prefix=<prefix>]
  migrate  [--auto] [--dry-run]
  resolve  <project>

JSON output on stdout for create / setup / migrate / resolve. Logs on stderr.
USAGE
            return 1
            ;;
        *) _wtm_err "unknown verb: $verb"; return 1 ;;
    esac
}

# When sourced (BASH_SOURCE[0] != $0), expose helpers without dispatching.
# Guard against set -u: BASH_SOURCE may be unset when sourced from very
# limited environments. Default to running main as if invoked directly.
if [[ "${BASH_SOURCE[0]:-$0}" == "$0" ]]; then
    main "$@"
fi
