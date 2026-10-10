#!/usr/bin/env bash
# worktree-config.sh - the one worktree.* config reader and the one linker call
# for every worktree creation path: the plugin WorktreeCreate hook
# (hooks/worktree-setup.sh) and `worktree-manager.sh setup` (/speculate).
#
# The reader asks the config schema (`fno config get`), never a shell parse of
# the TOML. Both shell parsers it replaced lost `auto_install = false`: yq's
# `//` treats boolean false as absent, and a YAML range matcher never opens on
# a TOML `[worktree]` table.

# wt_config <key> <default>: print worktree.<key>, or <default> when unset or
# when neither fno nor fno-py is on PATH (fno-py is the wheel's entry point, on
# PATH under `uv run`). Only the Python bool words are lowercased: the command
# keys (setup_command, test_command) keep their case.
wt_config() {
    local val="" cli=""
    if command -v fno >/dev/null 2>&1; then
        cli=fno
    elif command -v fno-py >/dev/null 2>&1; then
        cli=fno-py
    fi
    if [[ -n "$cli" ]]; then
        val="$("$cli" config get "worktree.$1" 2>/dev/null)" || val=""
    fi
    case "$val" in
        True) val=true ;;
        False) val=false ;;
        None) val="" ;;
    esac
    if [[ -n "$val" ]]; then
        printf '%s\n' "$val"
    else
        printf '%s\n' "$2"
    fi
}

# wt_link <worktree> <canonical>: run the checkout's own setup-worktree.sh so
# every creation path gets the same link set. Never fatal, and all output goes
# to stderr: the WorktreeCreate hook's stdout must stay one line. The checkout's
# copy, not the plugin's, because the linker also installs this repo's git
# hooks, which a foreign repo must not receive.
wt_link() {
    local linker="$1/scripts/setup/setup-worktree.sh"
    if [[ -f "$linker" ]]; then
        ( cd "$1" && CANONICAL="$2" WORKTREE="$1" bash "$linker" ) >&2 \
            || echo "Note: setup-worktree.sh exited non-zero; worktree usable but not fully linked" >&2
    else
        echo "Note: no scripts/setup/setup-worktree.sh in this checkout; shared state not linked" >&2
    fi
}
