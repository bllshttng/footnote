#!/usr/bin/env bash
# Daily throttle + background-detach helper for the eval-loop ignition:
# `fno observer sweep` (the sole producer of skill_eval_run_complete events)
# followed by `fno skill-diff tick` (the proposer that consumes them).
#
# Sourced by hooks/eval-sweep-session-start.sh (SessionStart: fire-only).
#
# Unlike reconcile (15-min, user-facing render), this fires DAILY (86400s) and
# renders nothing at session start - eval output is a background log/artifact,
# not a reminder. Its own stamp (.fno/.eval-sweep-stamp) keeps the two cadences
# independent. The window is claimed up-front so a burst of parallel sessions
# fires the sweep once, and the whole run is detached (nohup) so it never blocks
# session start. Best-effort throughout: a missing fno, missing corpus, or a
# sweep/tick error never propagates to the calling hook.
#
# Autonomy is untouched: the proposer stays at its config default `report`
# (dry-run) level. This helper only lights the ignition; it never flips level.

# Reuse reconcile's helpers verbatim (_reconcile_mtime, _reconcile_resolve_abi)
# by sourcing it - zero edits to reconcile's logic (Locked Decision 6).
_EVAL_SWEEP_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/reconcile-throttle.sh
source "$_EVAL_SWEEP_LIB_DIR/reconcile-throttle.sh" 2>/dev/null || return 0

# Throttle window in seconds (default 24h). Overridable for tests.
EVAL_SWEEP_THROTTLE_SECONDS="${EVAL_SWEEP_THROTTLE_SECONDS:-86400}"

# _eval_sweep_paused <abi_cmd>
# True (0) when the loops kill switch is active, so the fire is skipped. The
# eval sweep IS loop activity, so `fno loops pause-all` must stop it. Fail
# CLOSED: any output that does not begin with "not paused" / "expired" (a
# corrupt or unreadable state included) is treated as paused (Claude's
# Discretion 2 - single source of truth via the CLI, fail-closed on ambiguity).
_eval_sweep_paused() {
    local abi_cmd="$1" status
    status="$("$abi_cmd" loops status 2>/dev/null)" || return 0
    case "$status" in
        "not paused"* | "expired"*) return 1 ;;
        *) return 0 ;;
    esac
}

# eval_sweep_maybe_fire <repo_root>
#
# Launches a backgrounded, detached observer sweep -> skill-diff tick for both
# pilot skills iff the daily window has elapsed AND the loops are not paused.
# Always returns 0.
eval_sweep_maybe_fire() {
    local repo_root="${1:-$PWD}"
    # Only fire in an already-initialized project; never create .fno in a virgin dir.
    [[ -d "$repo_root/.fno" ]] || return 0
    local stamp="$repo_root/.fno/.eval-sweep-stamp"

    # Throttle: skip if the stamp is younger than the window. A missing stamp
    # (first-ever fire) is treated as "window elapsed" and fires once.
    if [[ -f "$stamp" ]]; then
        local now age
        now=$(date +%s)
        age=$(( now - $(_reconcile_mtime "$stamp") ))
        if (( age < EVAL_SWEEP_THROTTLE_SECONDS )); then
            return 0
        fi
    fi

    local abi_cmd
    abi_cmd="$(_reconcile_resolve_abi)" || return 0
    [[ -n "$abi_cmd" ]] || return 0

    # Honor the pause-all kill switch (fail-closed). Do NOT claim the stamp when
    # paused: an unpause should let the next session fire, not wait a full window.
    _eval_sweep_paused "$abi_cmd" && return 0

    # Claim the throttle window BEFORE launching so a parallel caller starting in
    # the same instant sees a fresh stamp and skips.
    : > "$stamp" 2>/dev/null || touch "$stamp" 2>/dev/null || true

    # Detach fully. Sweep MUST precede tick per skill (tick consumes the
    # run_complete the sweep emits); both-sweeps-then-both-ticks is fine as long
    # as each skill's sweep runs before its tick. Every step is `|| true`
    # best-effort so a non-zero sweep/tick never surfaces.
    nohup bash -c '
        cd "$1" 2>/dev/null || exit 0
        "$2" observer sweep --skill blueprint >/dev/null 2>&1 || true
        "$2" observer sweep --skill review    >/dev/null 2>&1 || true
        "$2" skill-diff tick --skill blueprint >/dev/null 2>&1 || true
        "$2" skill-diff tick --skill review    >/dev/null 2>&1 || true
    ' _ "$repo_root" "$abi_cmd" >/dev/null 2>&1 &
    disown 2>/dev/null || true

    return 0
}
