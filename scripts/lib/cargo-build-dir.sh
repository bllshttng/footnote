#!/usr/bin/env bash
# Remove a worktree's cargo build hash dir at worktree-removal time.
#
# Cargo writes intermediates OUTSIDE the checkout, at
# <build-base>/<h2>/<hash> under build.build-dir. Removing a worktree
# therefore orphans its hash dir until a later sweep reaches it. Resolution
# reads the workspace manifest, so the removal must happen BEFORE the
# checkout is deleted; every caller (hooks/worktree-remove.sh,
# scripts/setup/archive-worktree.sh, scripts/lib/worktree-lifecycle.sh)
# invokes this just before `git worktree remove`. Best-effort by contract: an
# unreadable resolution leaves the dir to the sweep, never fails the removal.
#
# The ownership answer (which base, which tag, which fingerprint) lives in
# crates/fno-agents/src/cargo_build_dirs.rs; this shim only forwards to the
# `reclaim remove-for` subcommand through the shared binary resolver.

# The fno-agents binary resolver, shared with the hooks. The canonical copy is
# hooks/lib/agents-bin.sh; this same-shape inline fallback covers a partial
# deploy whose hooks tree is not beside the scripts tree.
if [[ -f "${BASH_SOURCE[0]%/*}/../../hooks/lib/agents-bin.sh" ]]; then
    # shellcheck source=/dev/null
    source "${BASH_SOURCE[0]%/*}/../../hooks/lib/agents-bin.sh"
else
    fno_agents_bin() {
        local root="${1:-.}"
        if [[ -n "${FNO_AGENTS_BIN:-}" ]] && [[ -x "${FNO_AGENTS_BIN}" ]]; then
            printf '%s' "$FNO_AGENTS_BIN"
        elif [[ -x "$root/crates/fno-agents/target/release/fno-agents" ]]; then
            printf '%s' "$root/crates/fno-agents/target/release/fno-agents"
        elif [[ -x "$root/crates/fno-agents/target/debug/fno-agents" ]]; then
            printf '%s' "$root/crates/fno-agents/target/debug/fno-agents"
        else
            command -v fno-agents || printf ''
        fi
    }
fi

cargo_build_dir_remove_for_wt() {
    local wt="$1" bin
    # Best-effort delegation: a missing binary (FNO_AGENTS_BIN pointing
    # nowhere included) leaves the dir to the sweep and returns success, so
    # the caller's removal always proceeds.
    bin="$(fno_agents_bin "$wt")"
    if [[ -n "$bin" ]]; then
        "$bin" reclaim remove-for "$wt" >/dev/null 2>&1 || true
    fi
    return 0
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
