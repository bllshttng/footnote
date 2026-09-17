#!/usr/bin/env bash
# Worktree occupancy classification bridge.
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
    local bin="" login_rows="" remaining="" pid=""
    root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
    if [[ -n "${FNO_WT_OCCUPANCY_CMD:-}" ]]; then
        # Test seam: the hook battery stubs the classifier. Production sets nothing.
        # shellcheck disable=SC2086  # one pid per argv word, by contract
        out="$("$FNO_WT_OCCUPANCY_CMD" "$wt" $pids 2>/dev/null)" || rc=$?
    else
        # R6 in Rust (no new Python in scripts/lib): idle login shells answer
        # through `fno-agents occupancy-login` and never reach the classifier.
        # A probe failure just leaves every pid with the classifier, fail closed.
        if [[ -f "${root}/hooks/lib/agents-bin.sh" ]]; then
            # shellcheck source=/dev/null
            source "${root}/hooks/lib/agents-bin.sh"
        else
            fno_agents_bin() {
                local r="${1:-.}"
                if [[ -n "${FNO_AGENTS_BIN:-}" ]] && [[ -x "${FNO_AGENTS_BIN}" ]]; then
                    printf '%s' "$FNO_AGENTS_BIN"
                elif [[ -x "$r/crates/fno-agents/target/release/fno-agents" ]]; then
                    printf '%s' "$r/crates/fno-agents/target/release/fno-agents"
                elif [[ -x "$r/crates/fno-agents/target/debug/fno-agents" ]]; then
                    printf '%s' "$r/crates/fno-agents/target/debug/fno-agents"
                else
                    command -v fno-agents || printf ''
                fi
            }
        fi
        remaining="$pids"
        bin="$(fno_agents_bin "$root")"
        if [[ -n "$bin" ]]; then
            # shellcheck disable=SC2086  # one pid per argv word, by contract
            login_rows="$("$bin" occupancy-login $pids 2>/dev/null)" || login_rows=""
        fi
        if [[ -n "$login_rows" ]]; then
            remaining=""
            for pid in $pids; do
                if ! awk -F '\t' -v p="$pid" '$1 == p { found=1 } END { exit !found }' <<< "$login_rows"; then
                    remaining="$remaining $pid"
                fi
            done
            remaining="${remaining# }"
        fi
        # The house interpreter resolver, same as worktree-reapable.sh: prefer
        # the checkout venv so a stale installed `fno` never decides this.
        if [[ -z "${FNO_PYTHON:-}" && -f "${root}/scripts/lib/fno-python.sh" ]]; then
            # shellcheck source=/dev/null
            source "${root}/scripts/lib/fno-python.sh" && fno_python_init "$root"
        fi
        if [[ -z "${FNO_PYTHON:-}" || ! -d "${root}/cli/src" ]]; then
            rc=1
        elif [[ -n "$remaining" ]]; then
            # The classifier is a script beside this bridge (see its header):
            # the cli/src tree it composes from rides PYTHONPATH, and only the
            # pids the Rust lane did not answer reach it.
            # shellcheck disable=SC2086  # one pid per argv word, by contract
            out="$(PYTHONPATH="${root}/cli/src${PYTHONPATH:+:$PYTHONPATH}" \
                "$FNO_PYTHON" "${root}/scripts/lib/worktree_occupancy.py" "$wt" $remaining 2>/dev/null)" || rc=$?
        fi
        if [[ -n "$login_rows" && -n "$out" ]]; then
            out="${login_rows}"$'\n'"${out}"
        else
            out="${login_rows}${out}"
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
