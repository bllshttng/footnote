#!/usr/bin/env bash
# Shared throttle + background-detach helper for `fno backlog reconcile`.
#
# Sourced by two surfaces:
#   - hooks/reconcile-session-start.sh  (SessionStart: fire-and-render)
#   - hooks/megawalk-stop-hook.sh       (between loop iterations)
#
# Both share ONE throttle stamp (.fno/.reconcile-stamp) so a burst of
# parallel sessions, or a long autonomous loop advancing every few minutes,
# does not hammer `gh` with a reconcile per fire. The window is claimed
# up-front (stamp touched before launch) so concurrent callers see a fresh
# stamp and skip rather than double-firing.
#
# Reconcile runs in MUTATE mode (never --dry-run): writing the retro sentinel
# that downstream triage consumes is the whole point. It is detached with
# nohup so it survives the hook process exiting and never blocks session start
# or loop advance. Best-effort throughout: a missing `fno`, missing repo, or a
# reconcile error never propagates a non-zero exit to the calling hook.

# Throttle window in seconds (default 15 min). Overridable for tests.
RECONCILE_THROTTLE_SECONDS="${RECONCILE_THROTTLE_SECONDS:-900}"

# Resolve an `fno` runner. Hooks run in a minimal env where ~/.local/bin may
# not be on PATH. Prefer PATH, then the common user-install location. Echoes
# the command (or nothing) and returns non-zero when none is found.
_reconcile_resolve_abi() {
    if command -v fno >/dev/null 2>&1; then
        echo "fno"
        return 0
    fi
    if [[ -x "$HOME/.local/bin/fno-py" ]]; then
        echo "$HOME/.local/bin/fno-py"
        return 0
    fi
    return 1
}

# File mtime in epoch seconds, portable across macOS (stat -f) and GNU (stat -c).
_reconcile_mtime() {
    stat -f %m "$1" 2>/dev/null || stat -c %Y "$1" 2>/dev/null || echo 0
}

# reconcile_maybe_fire <repo_root>
#
# Launches a backgrounded, detached `fno backlog reconcile --json` (mutate
# mode) iff the throttle window has elapsed since the last fire. Result JSON is
# written atomically to .fno/.reconcile-result.json for the SessionStart
# hook to render on a later session. Always returns 0.
reconcile_maybe_fire() {
    local repo_root="${1:-$PWD}"
    # Only reconcile an already-initialized project; never create .fno in a virgin dir.
    [[ -d "$repo_root/.fno" ]] || return 0
    local footnote_dir="$repo_root/.fno"
    local stamp="$footnote_dir/.reconcile-stamp"
    local result="$footnote_dir/.reconcile-result.json"

    # Throttle: skip if the stamp is younger than the window.
    if [[ -f "$stamp" ]]; then
        local now age
        now=$(date +%s)
        age=$(( now - $(_reconcile_mtime "$stamp") ))
        if (( age < RECONCILE_THROTTLE_SECONDS )); then
            return 0
        fi
    fi

    local abi_cmd
    abi_cmd="$(_reconcile_resolve_abi)" || return 0
    [[ -n "$abi_cmd" ]] || return 0

    # Claim the throttle window BEFORE launching so a parallel caller starting
    # in the same instant sees a fresh stamp and skips.
    : > "$stamp" 2>/dev/null || touch "$stamp" 2>/dev/null || true

    # Detach fully: nohup + background + redirect so the reconcile outlives the
    # hook process and never blocks. Atomic result publish via tmp + mv.
    #
    # `fno retro run` and `fno backlog capture tidy` co-fire within the SAME
    # throttle window (both share the stamp claimed above). Sequenced AFTER
    # reconcile: reconcile closes drifted nodes and writes their retro-pending
    # sentinels, then `retro run` consumes those (plus any older pending ones) so
    # a GitHub web-UI merge - which drops no local event and never triggers a
    # human `/pr merged` - still gets its carveout/retro harvest within one
    # SessionStart+throttle window. retro run is the UNIVERSAL sentinel consumer;
    # it scopes each sentinel's review harvest to that sentinel's own pr_url repo
    # (foreign-repo sentinels file project=None, never mis-harvested against this
    # repo). Both retro run and tidy are best-effort (|| true): a failure never
    # affects the reconcile result publish and, per retro's own contract, a
    # failed harvest RETAINS the sentinel for the next window's retry.
    #
    # `fno retro drain-postmortems` also co-fires here (x-42f6 US3): a stuck
    # session's postmortem is decoupled from the post-merge `retro run` trigger,
    # so it drains within one throttle window of the next session start instead
    # of waiting for an unrelated PR to merge. Best-effort, sequenced last; dedup
    # by existing-node + consumed_at makes a race with a post-merge run safe.
    nohup bash -c '
        cd "$1" 2>/dev/null || exit 0
        "$2" backlog reconcile --json > "$3.tmp" 2>/dev/null \
            && mv -f "$3.tmp" "$3" 2>/dev/null
        "$2" retro run >/dev/null 2>&1 || true
        "$2" backlog capture tidy >/dev/null 2>&1 || true
        "$2" retro drain-postmortems >/dev/null 2>&1 || true
    ' _ "$repo_root" "$abi_cmd" "$result" >/dev/null 2>&1 &
    disown 2>/dev/null || true

    return 0
}
