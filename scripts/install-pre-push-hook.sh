#!/usr/bin/env bash
# install-pre-push-hook.sh - install the repo's destination-gating pre-push
# hook into this git repo's shared hooks directory.
#
# Idempotent. Safe to re-run. Symlinks <git-common-dir>/hooks/pre-push ->
# the canonical hooks/pre-push.sh shipped in this repo, backing up any real
# file it finds to <target>.backup.<UTC timestamp>.
#
# Targets the COMMON dir, not --git-dir: in a linked worktree --git-dir points
# at .git/worktrees/<name>, whose hooks directory is empty and never consulted,
# while the common dir holds the repo-local hooks every worktree shares.
#
# core.hooksPath replaces a repo's hooks directory entirely. When it is set,
# the file this script writes may never run, so the post-install step compares
# the install target against `git rev-parse --git-path hooks/pre-push` and,
# when they differ, names the path that runs first instead of reporting the
# guard as active.
#
# Flags:
#   --check   Report only, install nothing. Exit 0 when a destination-gating
#             hook is in place; exit 1 otherwise. Status word on stdout:
#             installed | absent | legacy | foreign | not-a-repo.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HOOK_SOURCE="$REPO_ROOT/hooks/pre-push.sh"

abs_path() {
    # git rev-parse prints paths relative to cwd; anchor them.
    case "$1" in
        /*) printf '%s\n' "$1" ;;
        *) printf '%s/%s\n' "$(pwd)" "$1" ;;
    esac
}

if [[ "${1:-}" == "--check" ]]; then
    COMMON_DIR="$(git rev-parse --git-common-dir 2>/dev/null)" || {
        echo "pre-push: not-a-repo (not inside a git repository)"
        exit 1
    }
    TARGET="$(abs_path "$COMMON_DIR")/hooks/pre-push"
    if [[ ! -e "$TARGET" ]]; then
        echo "pre-push: absent (no hook at $TARGET)"
        exit 1
    fi
    if grep -q 'symbolic-ref' "$TARGET" 2>/dev/null; then
        echo "pre-push: legacy (hook at $TARGET gates on the pushing checkout's branch: symbolic-ref)"
        exit 1
    fi
    if [[ -L "$TARGET" && "$(readlink "$TARGET")" == "$HOOK_SOURCE" ]] \
        || grep -q 'fno-pre-push-destination-gate' "$TARGET" 2>/dev/null; then
        echo "pre-push: installed ($TARGET)"
        exit 0
    fi
    echo "pre-push: foreign (hook at $TARGET is neither this gate nor the legacy one)"
    exit 1
fi

if [[ ! -f "$HOOK_SOURCE" ]]; then
    echo "install-pre-push-hook: source hook not found at $HOOK_SOURCE" >&2
    exit 1
fi

COMMON_DIR="$(git rev-parse --git-common-dir 2>/dev/null)" || {
    echo "install-pre-push-hook: not inside a git repository" >&2
    exit 1
}
HOOKS_DIR="$(abs_path "$COMMON_DIR")/hooks"
HOOK_TARGET="$HOOKS_DIR/pre-push"
mkdir -p "$HOOKS_DIR"

# Idempotent symlink: if target points to source, do nothing.
if [[ -L "$HOOK_TARGET" ]]; then
    CURRENT=$(readlink "$HOOK_TARGET")
    if [[ "$CURRENT" == "$HOOK_SOURCE" ]]; then
        echo "install-pre-push-hook: already installed (no change)" >&2
        exit 0
    fi
    rm "$HOOK_TARGET"
elif [[ -e "$HOOK_TARGET" ]]; then
    # Existing real file. Back up before replacing so we never lose user state.
    BACKUP="${HOOK_TARGET}.backup.$(date -u +%Y%m%dT%H%M%SZ)"
    mv "$HOOK_TARGET" "$BACKUP"
    echo "install-pre-push-hook: backed up existing hook to $BACKUP" >&2
fi

ln -s "$HOOK_SOURCE" "$HOOK_TARGET"
echo "install-pre-push-hook: installed $HOOK_TARGET -> $HOOK_SOURCE" >&2

# Reachability honesty: with core.hooksPath set elsewhere, git runs that
# directory's pre-push first and ours only if that file re-dispatches here.
EFFECTIVE="$(abs_path "$(git rev-parse --git-path hooks/pre-push)")"
if [[ "$EFFECTIVE" != "$HOOK_TARGET" ]]; then
    echo "install-pre-push-hook: guard NOT reported active - core.hooksPath makes" >&2
    echo "  $EFFECTIVE run first; it may or may not re-dispatch to $HOOK_TARGET." >&2
fi
