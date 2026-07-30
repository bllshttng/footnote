#!/usr/bin/env bash
# fno-python.sh - resolve an interpreter that can run the `-m fno.<mod>` helpers.
#
#   source "${REPO_ROOT}/scripts/lib/fno-python.sh"
#   fno_python_init "$REPO_ROOT"      # sets FNO_PYTHON
#   "$FNO_PYTHON" -m fno.cost._register ...
#
# Callers keep their own PYTHONPATH export: it must still happen when a partial
# deploy dropped this file, so it cannot live here.
#
# A bare `python3` is whatever is first on PATH. One without fno's deps dies in
# fno.paths on `import fno.config`, and the ledger writers route stderr to a log,
# so the row is dropped silently. `--help` never reaches that import, so a smoke
# check that only runs --help reports healthy.
#
# The CALLER resolves the interpreter, never the callee: `python3 -m
# fno.cost._register` imports fno.cost (-> fno.events -> yaml) before the
# module's __main__ runs, so a module that re-execs itself is dead code on the
# interpreters that need it most. fno-agents finalize resolves the same venv the
# same way; this keeps bash and Rust on one rule.

# Always succeeds: falls back to `python3` so a missing venv degrades rather
# than breaking the caller.
fno_python_init() {
    local root="${1:-}"
    [[ -n "$root" ]] || root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"

    # A LINKED WORKTREE carries cli/src but no cli/.venv, so the canonical main
    # worktree is what resolves. Gating on a co-located cli/src/fno (the guard
    # finalize's footnote_venv applies) keeps a foreign project's cli/.venv,
    # where `import fno` would fail, from trading one silent drop for another.
    local canonical cand
    canonical="$(git -C "$root" worktree list --porcelain 2>/dev/null \
        | awk 'NR==1{sub(/^worktree /,"");print}')"
    for cand in "$root" "$canonical"; do
        [[ -n "$cand" ]] || continue
        if [[ -x "${cand}/cli/.venv/bin/python3" && -f "${cand}/cli/src/fno/__init__.py" ]]; then
            export FNO_PYTHON="${cand}/cli/.venv/bin/python3"
            return 0
        fi
    done

    # No checkout: the interpreter behind the installed `fno-py` console script.
    # PATH alone is NOT enough - on a cargo-only install only the Rust mux
    # (~/.cargo/bin/fno) is on PATH and fno-py is not, and that is precisely the
    # machine with no checkout venv to fall back on. So also probe uv's default
    # tool bin dir and the tool venv itself. (`fno` is the Rust binary and has no
    # shebang to read; LC_ALL=C keeps sed off a locale decode error should
    # something non-text land on PATH under that name.)
    local shim interp
    for shim in "$(command -v fno-py 2>/dev/null || true)" \
                "${HOME}/.local/bin/fno-py" \
                "${XDG_DATA_HOME:-${HOME}/.local/share}/uv/tools/fno/bin/fno-py"; do
        [[ -n "$shim" && -f "$shim" ]] || continue
        interp="$(LC_ALL=C sed -n '1{s/^#!//p;q;}' "$shim" 2>/dev/null | awk '{print $1}')"
        if [[ -n "$interp" && -x "$interp" ]]; then
            export FNO_PYTHON="$interp"
            return 0
        fi
    done

    export FNO_PYTHON="python3"   # correct where python3 already has the deps
}
