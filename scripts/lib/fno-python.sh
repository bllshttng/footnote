#!/usr/bin/env bash
# fno-python.sh - resolve an interpreter that can run the `-m fno.<mod>` helpers.
#
# Source this, don't execute it. `fno_python_init [repo_root]` sets FNO_PYTHON
# and, in a source checkout, exports PYTHONPATH so the helpers import pre-install.
#
#   source "${REPO_ROOT}/scripts/lib/fno-python.sh"
#   fno_python_init "$REPO_ROOT"
#   "$FNO_PYTHON" -m fno.cost._register --type think ...
#
# WHY: a bare `python3 -m fno.<mod>` runs under whatever python is first on
# PATH. When that python lacks fno's third-party deps, every settings-touching
# helper dies inside fno.paths on `import fno.config`, and these callers route
# stderr to a log or /dev/null - so the ledger row is dropped with nothing
# surfacing at the time of loss. Reproduced in production: `~/.local/bin/python3`
# has yaml but not pydantic/tomli_w, and `fno.cost._register` exited 1 while the
# caller carried on. `--help` never reaches the import, so a smoke check that
# only runs --help reports healthy.
#
# The CALLER resolves the interpreter, not the callee: `python3 -m fno.cost._register`
# must import fno.cost (-> fno.events -> yaml) before any code in the module's
# __main__ can run, so a module that re-execs itself is dead code on exactly the
# interpreters that need it most (macOS /usr/bin/python3 has no yaml). This
# mirrors what fno-agents finalize already does for the same three helpers.

# Resolve FNO_PYTHON. Always succeeds: falls back to `python3` (the historical
# behavior) so a missing venv degrades rather than breaking the caller outright.
fno_python_init() {
    local root="${1:-}"
    [[ -n "$root" ]] || root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"

    # Package source onto PYTHONPATH so a checkout works before `uv tool install`.
    if [[ -f "${root}/cli/src/fno/__init__.py" ]]; then
        export PYTHONPATH="${root}/cli/src${PYTHONPATH:+:${PYTHONPATH}}"
    fi

    # 1. The checkout's venv. A LINKED WORKTREE carries cli/src but no cli/.venv,
    #    so the canonical main worktree is the one that resolves it.
    local canonical cand
    canonical="$(git -C "$root" worktree list --porcelain 2>/dev/null \
        | awk 'NR==1{sub(/^worktree /,"");print}')"
    for cand in "$root" "$canonical"; do
        [[ -n "$cand" ]] || continue
        # Gate on co-located package source: a foreign project carrying its own
        # cli/.venv without fno installed would trade one silent drop for
        # another (the same guard finalize applies in footnote_venv).
        if [[ -x "${cand}/cli/.venv/bin/python3" && -f "${cand}/cli/src/fno/__init__.py" ]]; then
            export FNO_PYTHON="${cand}/cli/.venv/bin/python3"
            return 0
        fi
    done

    # 2. No checkout: the interpreter behind the installed `fno-py` console
    #    script, read from its shebang. (`fno` itself is the Rust binary and has
    #    no shebang to read; LC_ALL=C keeps sed off a locale decode error if
    #    something non-text ever lands on PATH under that name.)
    local shim interp
    shim="$(command -v fno-py 2>/dev/null || true)"
    if [[ -n "$shim" ]]; then
        interp="$(LC_ALL=C sed -n '1{s/^#!//p;q;}' "$shim" 2>/dev/null | awk '{print $1}')"
        if [[ -n "$interp" && -x "$interp" ]]; then
            export FNO_PYTHON="$interp"
            return 0
        fi
    fi

    # 3. Whatever is on PATH. Correct on a machine whose python3 has the deps.
    export FNO_PYTHON="python3"
}
