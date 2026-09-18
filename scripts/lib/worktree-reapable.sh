#!/usr/bin/env bash
# worktree-reapable.sh - the ONE "is removing this worktree safe?" question.
#
#   source "${REPO_ROOT}/scripts/lib/worktree-reapable.sh"
#   if wt_reapable "$wt"; then ... fi        # receipt lands in WT_REAPABLE_LINE
#
# Both bash removal call sites (the `--merged` sweep in worktree-lifecycle.sh
# and the strict check in archive-worktree.sh) used to run their own
# `git status --porcelain` and block on any output. So did the Rust row-GC
# probe. Three implementations of one question is a defect class this repo
# documents, so all three now route to ONE classifier, and an equivalence
# test pins that they agree.
#
# THE CLASSIFIER. crates/fno-agents/src/worktree_reapable.rs (the done-node
# salvage arm deleted
# the Python leg): blocks on modified tracked content, untracked files, and
# unmerged conflicts. A tracked file missing from disk never blocks - HEAD
# holds its content - and neither do the symlinks setup-worktree.sh writes
# (`discounted=` names them).
#
# FAIL CLOSED, AND ON A POSITIVE MARKER. Permission requires BOTH exit 0 and a
# literal `reapable=yes` on stdout. An absence of "no" is not a yes: a stale
# installed CLI that predates the verb exits 2 with no receipt at all, which is
# indistinguishable from any other non-answer at this layer. Every unknown
# degrades to "not reapable", which is exactly the historical behaviour.
#
# FLAGS (caller env, both default off):
#   WT_REAPABLE_ALLOW_UNBORN=1  lift the setup-window refusal for a tree a
#                               human NAMED (the orphan-recovery path). Bulk
#                               sweeps never set it.
#   WT_REAPABLE_DONE_NODE=1     turn on the done-node arm: a tree whose node
#                               reads done/superseded (or whose branch is
#                               merged) may go with its branch kept, untracked
#                               files salvaged by the CALLER first. Only the
#                               merged sweep's gate call sets it.

# Set by wt_reapable to the verb's receipt line (or a synthesised one).
WT_REAPABLE_LINE=""

# One attempt's reading: 0 a real yes, 1 a real no, 2 no answer at all.
_wt_reapable_verdict() {
    local rc="$1" out="$2"
    [[ "$rc" -eq 0 && "$out" == *"reapable=yes"* ]] && return 0
    [[ "$rc" -eq 1 && "$out" == reapable=no* ]] && return 1
    return 2
}

wt_reapable() {
    local target="${1:-}"
    WT_REAPABLE_LINE=""
    if [[ -z "$target" || ! -d "$target" ]]; then
        WT_REAPABLE_LINE="reapable=no reason=probe-failed detail=no-such-directory"
        return 1
    fi

    # Anchor on THIS FILE, never on the target: the target is frequently a
    # worktree of some other repo (or a bare temp repo in tests) with no
    # build of anything.
    local root
    root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

    # The gate binary: an explicit override first ($FNO_AGENTS_BIN, the same
    # name the Python runtime honors - a fixture or an operator who set it
    # meant it), then the checkout's own cargo build (release then debug, the
    # same order fno.rust_binary uses), then PATH. A repo binary outranks an
    # installed one so a dev tree and CI always ask THEIR gate, never a
    # deployed older one.
    local bin="" cand
    if [[ -n "${FNO_AGENTS_BIN:-}" && -x "${FNO_AGENTS_BIN:-}" ]]; then
        bin="${FNO_AGENTS_BIN}"
    else
        for cand in \
            "$root/crates/fno-agents/target/release/fno-agents" \
            "$root/target/release/fno-agents" \
            "$root/crates/fno-agents/target/debug/fno-agents" \
            "$root/target/debug/fno-agents"
        do
            if [[ -x "$cand" ]]; then bin="$cand"; break; fi
        done
        if [[ -z "$bin" ]]; then
            cand="$(command -v fno-agents 2>/dev/null)" && bin="$cand"
        fi
    fi
    if [[ -z "$bin" ]]; then
        cand="$(command -v fno-agents 2>/dev/null)" && bin="$cand"
    fi

    local flags=""
    # shellcheck disable=SC2086  # empty flags must vanish, not arrive empty
    [[ "${WT_REAPABLE_ALLOW_UNBORN:-0}" == "1" ]] && flags="$flags --allow-unborn"
    [[ "${WT_REAPABLE_DONE_NODE:-0}" == "1" ]] && flags="$flags --done-node"

    local out="" rc=0 verdict=2
    if [[ -n "$bin" ]]; then
        # shellcheck disable=SC2086
        out="$("$bin" worktree-reapable $flags "$target" 2>/dev/null)" || rc=$?
        verdict=0; _wt_reapable_verdict "$rc" "$out" || verdict=$?
    fi

    # FALL THROUGH, DO NOT STOP AT THE FIRST SILENCE. A checkout with no build
    # and no fno-agents on PATH still has the installed `fno`, whose typer
    # leaf execs its own binary. An installed fno OLDER than the gate refuses
    # an unknown --done-node flag with no receipt: that reads as no answer
    # (fail closed), never as permission.
    if [[ "$verdict" -eq 2 ]] && command -v fno >/dev/null 2>&1; then
        rc=0
        # shellcheck disable=SC2086
        out="$(fno agents workspace worktree reapable $flags "$target" 2>/dev/null \
            || fno workspace worktree reapable $flags "$target" 2>/dev/null)" || rc=$?
        verdict=0; _wt_reapable_verdict "$rc" "$out" || verdict=$?
    fi

    # 0 permission (positive marker AND clean exit), 1 a receipt that says no.
    if [[ "$verdict" -le 1 ]]; then
        WT_REAPABLE_LINE="$out"
        return "$verdict"
    fi
    WT_REAPABLE_LINE="reapable=no reason=probe-failed detail=verb-unavailable(rc=$rc)"
    return 1
}
