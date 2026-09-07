#!/usr/bin/env bash
# rooted-writers.sh - foreign live writers rooted in a checkout, attributed to
# an owning agent-registry row. Consumed by hooks/context-nudge.sh section 7b
# so the flush nudge never advises committing another session's mid-flight
# work (x-299b: a king was urged to commit 351 lines belonging to a live codex
# worker that had edited canonical before entering its own worktree).
#
# foreign_rooted_writers <checkout> [<my-harness-session-id>]
#   stdout: one line per foreign writer, tab separated:
#     <pid>\t<registry-name>\t<harness_session_id>
#   exit 0: the measurement succeeded (empty stdout means none)
#   exit 2: the measurement could not be made; the caller must fail closed.
#     An unmeasurable process table is never read as "nobody is here" - the
#     same direction as the sweep's kept (process-snapshot-unreadable).
#
# The predicate is: rooted HERE (cwd under the checkout, or argv carrying it,
# via worktree-lifecycle.sh's _wt_pids), not MINE (ancestry walk drops the
# caller's ancestors - the harness that spawned the hook - and its
# descendants), and claimed by a registry row in an ACTIVE status (anything
# but exited/orphaned/failed/permanent_dead - a worker mid-turn projects
# "busy", not always "live") that owns a pid (cli/src/fno/agents/registry.py
# writes pid only when the writer owns the process, which is the
# subprocess-dispatch lane that edits canonical before moving into a
# worktree).
#
# ponytail: a foreign writer with NO pid-bearing registry row is invisible
# here - today that is the operator's shell and every pane/thread worker
# (36 of 37 live rows). Widening the net means inventing a second identity
# source; the context_flush_refused event stream is what surfaces anything
# this misses.
#
# pid_start_time is read but not compared against the live process: a reused
# pid would have to be reused by a process currently rooted in this exact
# checkout, and the failure direction is a refusal, which is the safe one.

foreign_rooted_writers() {
    local checkout="$1" my_sid="${2:-}" lifecycle
    lifecycle="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/worktree-lifecycle.sh"
    [[ -f "$lifecycle" ]] || return 2
    # worktree-lifecycle.sh's `case "${1:-status}"` main dispatch is unguarded,
    # so a blanket source would run its status sweep. The established reuse
    # (tests/hooks/test_worktree_remove_lifecycle.sh) extracts the two
    # functions instead.
    eval "$(sed -n '/^_wt_refresh_cwd_snapshot()/,/^}/p; /^_wt_pids()/,/^}/p' "$lifecycle" 2>/dev/null)" || return 2
    declare -F _wt_refresh_cwd_snapshot >/dev/null 2>&1 || return 2
    declare -F _wt_pids >/dev/null 2>&1 || return 2
    _wt_refresh_cwd_snapshot || return 2
    local rooted rc
    rooted="$(_wt_pids "$checkout")"; rc=$?
    [[ "$rc" -eq 0 ]] || return 2
    [[ -n "$rooted" ]] || return 0

    # Ancestry self-exclusion from one pid->ppid snapshot: drop a candidate
    # that is an ancestor of $$ (the harness that spawned the hook; without
    # this the guard refuses because the advisor exists) and one that is a
    # descendant of $$ (something the hook's own session runs). Same-session
    # workers not in either set are dropped later by the registry join.
    local ps_snap survivors
    ps_snap="$(ps -Ao pid=,ppid= 2>/dev/null)" || return 2
    survivors="$(awk -v me="$$" '
        FNR==NR {
            if ($0 == "__FNO_PS_DONE__") { complete = 1; next }
            p[$1] = $2; next
        }
        {
            pid = $1
            if (!complete) { print "__PS_SNAPSHOT_INCOMPLETE__"; next }
            # Absent from the snapshot = already dead: the pgrep lane of
            # _wt_pids catches its own transient awk child (the path rides in
            # argv as -v root and -v logical), and a dead pid owns no work.
            if (!(pid in p)) next
            cur = me; anc = 0; n = 0
            while ((cur in p) && n++ < 4096) { cur = p[cur]; if (cur == pid) { anc = 1; break } }
            if (anc) next
            cur = pid; desc = 0; n = 0
            while ((cur in p) && n++ < 4096) { if (cur == me) { desc = 1; break } cur = p[cur] }
            if (desc) next
            print
        }' <(printf '%s\n' "$ps_snap"; printf '%s\n' '__FNO_PS_DONE__') <(printf '%s\n' "$rooted"))"
    [[ "$survivors" == *__PS_SNAPSHOT_INCOMPLETE__* ]] && return 2
    [[ -n "$survivors" ]] || return 0

    # Registry join. A MISSING registry is an empty one: no agent has ever
    # registered, so every rooted process is an operator shell or editor and
    # nothing can be claimed as a foreign agent writer. A registry that exists
    # but cannot be parsed is an unmeasurable join -> exit 2.
    local reg
    reg="${STATE_DIR:-${HOME:-}/.fno}/agents/registry.json"
    [[ -f "$reg" ]] || return 0
    jq -r --arg sid "$my_sid" --arg pids "$survivors" '
        .agents[]?
        | select((.status // "live") != "exited" and (.status // "live") != "orphaned"
                 and (.status // "live") != "failed" and (.status // "live") != "permanent_dead")
        | select(.pid != null)
        | select(.harness_session_id != $sid)
        | (.pid | tostring) as $p
        | select(($pids | split("\n") | index($p)) != null)
        | [(.pid | tostring), (.name // "?"), (.harness_session_id // "?")] | @tsv
    ' "$reg" 2>/dev/null || return 2
    return 0
}
