#!/usr/bin/env bash
# Worktree occupancy classification bridge (x-0396).
#
# wt_classify_pids <worktree> <newline-separated pids>
#
# Prints one tab-separated row per pid:
#   pid <tab> verdict <tab> action <tab> job <tab> reason <tab> cmd
# verdict is `holds` or `inert`; action is `keep`, `terminate` or `retire`.
#
# Fail closed: a non-zero classifier exit, a missing interpreter, or a row
# count that differs from the pid count reads every pid as
# `holds keep - classifier unavailable -`. A tree with any holds row is kept.
# Absence of a recognised holder is never proof a tree is free.

_wt_occupancy_failclosed() {
    local pid
    while IFS= read -r pid; do
        [[ -z "$pid" ]] && continue
        printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$pid" holds keep - "classifier unavailable" -
    done <<< "$1"
}

wt_classify_pids() {
    local wt="$1" pids="$2" root out="" rc=0 n_pids=0 n_rows=0
    root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
    if [[ -n "${FNO_WT_OCCUPANCY_CMD:-}" ]]; then
        # Test seam: the hook battery stubs the classifier. Production sets nothing.
        # shellcheck disable=SC2086  # one pid per argv word, by contract
        out="$("$FNO_WT_OCCUPANCY_CMD" "$wt" $pids 2>/dev/null)" || rc=$?
    else
        # The house interpreter resolver, same as worktree-reapable.sh: prefer
        # the checkout venv so a stale installed `fno` never decides this.
        if [[ -z "${FNO_PYTHON:-}" && -f "${root}/scripts/lib/fno-python.sh" ]]; then
            # shellcheck source=/dev/null
            source "${root}/scripts/lib/fno-python.sh" && fno_python_init "$root"
        fi
        if [[ -z "${FNO_PYTHON:-}" || ! -d "${root}/cli/src" ]]; then
            rc=1
        else
            # The classifier is a script beside this bridge (see its header):
            # the cli/src tree it composes from rides PYTHONPATH.
            # shellcheck disable=SC2086  # one pid per argv word, by contract
            out="$(PYTHONPATH="${root}/cli/src${PYTHONPATH:+:$PYTHONPATH}" \
                "$FNO_PYTHON" "${root}/scripts/lib/worktree_occupancy.py" "$wt" $pids 2>/dev/null)" || rc=$?
        fi
    fi
    n_pids="$(printf '%s\n' "$pids" | grep -c .)"
    if [[ "$rc" -eq 0 && -n "$out" ]]; then
        n_rows="$(printf '%s\n' "$out" | grep -c .)"
    fi
    if [[ "$rc" -ne 0 || "$n_rows" -ne "$n_pids" ]]; then
        _wt_occupancy_failclosed "$pids"
        return 0
    fi
    printf '%s\n' "$out"
}

# The receipt the sweep prints beside a kept or would-archive tree: one
# indented line per classified hit.
wt_occupancy_print_rows() {
    printf '%s\n' "$1" | awk -F '\t' 'NF >= 6 {
        cmd = $6
        if (length(cmd) > 80) cmd = substr(cmd, 1, 80)
        printf "    %s %s %s | %s\n", $1, $2, $5, cmd
    }'
}
