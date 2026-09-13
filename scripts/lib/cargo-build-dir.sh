#!/usr/bin/env bash
# Remove a worktree's cargo build hash dir at worktree-removal time.
#
# Cargo writes intermediates OUTSIDE the checkout, at
# <build-base>/<h2>/<hash> under build.build-dir. Removing a worktree
# therefore orphans its hash dir until a later sweep reaches it. Resolution
# reads the workspace manifest, so the removal must happen BEFORE the
# checkout is deleted; every caller (hooks/worktree-remove.sh,
# scripts/setup/archive-worktree.sh, scripts/lib/worktree-lifecycle.sh,
# crates/fno-agents merge_reap) invokes this just before `git worktree
# remove`. Best-effort by contract: an unreadable resolution leaves the dir
# to the sweep, never fails the removal.

# Where cargo intermediates live: FNO_CARGO_TARGETS_BASE override, else
# config.paths.cargo_targets_base, else <state>/cargo-build. Mirrors
# _cargo_build_base in worktree-lifecycle.sh, kept local because that script
# is a command dispatcher and cannot be sourced.
cargo_build_dir_base() {
    local raw=""
    if [[ -n "${FNO_CARGO_TARGETS_BASE:-}" ]]; then
        printf '%s\n' "${FNO_CARGO_TARGETS_BASE/#\~/$HOME}"
        return 0
    fi
    if command -v fno >/dev/null 2>&1; then
        raw="$(fno config get config.paths.cargo_targets_base 2>/dev/null || true)"
    fi
    [[ "$raw" == "null" || -z "$raw" ]] && raw="${STATE_DIR:-$HOME/.fno}/cargo-build"
    printf '%s\n' "${raw/#\~/$HOME}"
}

cargo_build_dir_remove_for_wt() {
    local wt="$1" manifest resolved resolved_p base_p found=0
    base_p="$(cd -- "$(cargo_build_dir_base)" 2>/dev/null && pwd -P)" || return 1
    for manifest in "$wt"/crates/*/Cargo.toml; do
        [[ -f "$manifest" ]] || continue
        resolved="$(cargo metadata --format-version 1 --no-deps --manifest-path "$manifest" 2>/dev/null \
            | grep -o '"build_directory"[[:space:]]*:[[:space:]]*"[^"]*"' \
            | sed 's/.*:[[:space:]]*"//; s/"$//')" || continue
        [[ -d "$resolved" ]] || continue
        # Owned iff under the managed build base AND carrying cargo's
        # CACHEDIR.TAG - the same two conjuncts the sweep deletes under.
        # Both sides normalised through pwd -P: /tmp is a symlink to
        # /private/tmp, and a logical path never matches a physical prefix.
        resolved_p="$(cd -- "$resolved" 2>/dev/null && pwd -P)" || continue
        [[ -f "$resolved_p/CACHEDIR.TAG" ]] || continue
        case "$resolved_p/" in
            "$base_p/"*) ;;
            *) continue ;;
        esac
        # Sanctioned disposable delete (docs/architecture/disposable-deletes.md):
        # a bare rm on a trash-aliased host relocates the bytes instead of
        # reclaiming them.
        if { command -p rm -rf "$resolved_p" 2>/dev/null || /bin/rm -rf "$resolved_p"; } && [[ ! -e "$resolved_p" ]]; then
            found=1
        fi
    done
    [[ "$found" -eq 1 ]]
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    case "${1:-}" in
        remove-for)
            [[ -n "${2:-}" ]] || { echo "usage: cargo-build-dir.sh remove-for <worktree>" >&2; exit 2; }
            cargo_build_dir_remove_for_wt "$2"
            ;;
        *)
            echo "usage: cargo-build-dir.sh remove-for <worktree>" >&2
            exit 2
            ;;
    esac
fi
