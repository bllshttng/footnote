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
# documents, so all three now route here, and an equivalence test pins that
# they agree.
#
# WHY THE ANSWER CHANGED. A tracked file missing from disk carries no
# unrecoverable information: HEAD holds its content. Blocking on it protects
# nothing and, measured 2026-08-13, blocked 17 of the 20 worktrees the sweep
# called dirty. The classifier (cli/src/fno/worktree_reapable.py) blocks on
# modified tracked content, untracked files, and unmerged conflicts only.
#
# FAIL CLOSED, AND ON A POSITIVE MARKER. Permission requires BOTH exit 0 and a
# literal `reapable=yes` on stdout. An absence of "no" is not a yes: a stale
# installed CLI that predates the verb exits 2 with no receipt at all, which is
# indistinguishable from any other non-answer at this layer. Every unknown
# degrades to "not reapable", which is exactly today's behaviour.

# Set by wt_reapable to the verb's receipt line (or a synthesised one).
WT_REAPABLE_LINE=""

wt_reapable() {
    local target="${1:-}"
    WT_REAPABLE_LINE=""
    if [[ -z "$target" || ! -d "$target" ]]; then
        WT_REAPABLE_LINE="reapable=no reason=probe-failed detail=no-such-directory"
        return 1
    fi

    # Anchor on THIS FILE, never on the target. The classifier belongs to
    # footnote, and the target is frequently a worktree of some other repo (or,
    # in tests, a bare temp repo) with no cli/src at all. Resolving from the
    # target silently skipped the verb and fell through to whatever `fno`
    # happened to be installed.
    local root
    root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

    # The house interpreter resolver: prefers the checkout venv, so a stale
    # installed `fno` never decides this. Sourced lazily because a partial
    # deploy may have dropped it, and that must degrade rather than break.
    if [[ -z "${FNO_PYTHON:-}" && -f "${root}/scripts/lib/fno-python.sh" ]]; then
        # shellcheck source=/dev/null
        source "${root}/scripts/lib/fno-python.sh" && fno_python_init "$root"
    fi

    local out rc=0
    if [[ -n "${FNO_PYTHON:-}" && -d "${root}/cli/src" ]]; then
        out="$(PYTHONPATH="${root}/cli/src${PYTHONPATH:+:$PYTHONPATH}" \
            "$FNO_PYTHON" -m fno.cli worktree reapable "$target" 2>/dev/null)" || rc=$?
    elif command -v fno >/dev/null 2>&1; then
        out="$(fno worktree reapable "$target" 2>/dev/null)" || rc=$?
    else
        WT_REAPABLE_LINE="reapable=no reason=probe-failed detail=no-fno-cli"
        return 1
    fi

    # Exit 1 with a receipt is a real "blocked"; report it verbatim.
    if [[ "$rc" -eq 1 && "$out" == reapable=no* ]]; then
        WT_REAPABLE_LINE="$out"
        return 1
    fi
    # Permission needs the positive marker AND a clean exit.
    if [[ "$rc" -eq 0 && "$out" == *"reapable=yes"* ]]; then
        WT_REAPABLE_LINE="$out"
        return 0
    fi
    WT_REAPABLE_LINE="reapable=no reason=probe-failed detail=verb-unavailable(rc=$rc)"
    return 1
}
