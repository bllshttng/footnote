#!/usr/bin/env bash
# Daily throttle + background-detach helper for the eval-loop ignition:
# `fno observer sweep` (the sole producer of skill_eval_run_complete events)
# followed by `fno skill-diff tick` (the proposer that consumes them).
#
# Sourced by hooks/eval-sweep-session-start.sh (SessionStart: fire-only).
#
# Unlike reconcile (15-min, user-facing render), this fires DAILY (86400s) and
# renders nothing at session start - eval output is a background log/artifact,
# not a reminder. The throttle stamp and singleton claim both resolve against
# the CANONICAL repo root (not the session cwd), so one sweep fires per repo
# per day regardless of how many worktrees start a session (x-dbdf). Its own
# stamp (.fno/.eval-sweep-stamp) keeps the two cadences independent. The whole
# run is detached (nohup), bounded per stage (timeout), and logged to
# .fno/logs/eval-sweep.log so a wedge dies and is diagnosable instead of
# accumulating as an orphan. Best-effort throughout: a missing fno, missing
# corpus, or a sweep/tick error never propagates to the calling hook.
#
# Autonomy is untouched: the proposer stays at its config default `report`
# (dry-run) level. This helper only lights the ignition; it never flips level.

# Reuse reconcile's _reconcile_resolve_fno verbatim by sourcing it - zero edits
# to reconcile's logic (Locked Decision 6). We do NOT reuse _reconcile_mtime: it
# is BSD-first (stat -f %m) and returns non-numeric garbage under GNU coreutils,
# so this file carries its own GNU-first _eval_sweep_mtime.
_EVAL_SWEEP_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/reconcile-throttle.sh
source "$_EVAL_SWEEP_LIB_DIR/reconcile-throttle.sh" 2>/dev/null || return 0

# Throttle window in seconds (default 24h). Overridable for tests.
EVAL_SWEEP_THROTTLE_SECONDS="${EVAL_SWEEP_THROTTLE_SECONDS:-86400}"
# Hard per-stage time bound (default 300s). A wedged sweep dies at this bound.
EVAL_SWEEP_STAGE_TIMEOUT="${EVAL_SWEEP_STAGE_TIMEOUT:-300}"
# Singleton claim TTL: self-frees a crashed run within this window.
EVAL_SWEEP_CLAIM_TTL="${EVAL_SWEEP_CLAIM_TTL:-30m}"
# Log truncated when it grows past this many bytes (keep the file small).
EVAL_SWEEP_LOG_MAX_BYTES="${EVAL_SWEEP_LOG_MAX_BYTES:-1048576}"

# _eval_sweep_mtime <path>
# File mtime in epoch seconds. GNU (stat -c) FIRST so Linux never reaches BSD's
# `stat -f %m`, which on GNU coreutils prints non-numeric output (not a clean
# failure) and would crash the caller's arithmetic under `set -u`. Scrubbed to
# digits so a non-numeric fallback can never break `$(( ))`.
_eval_sweep_mtime() {
    local m
    m=$(stat -c %Y "$1" 2>/dev/null || stat -f %m "$1" 2>/dev/null || echo 0)
    m=${m//[!0-9]/}
    echo "${m:-0}"
}

# _eval_sweep_canonical_root <dir>
# The canonical checkout root for <dir>, so all worktrees of one repo share a
# stamp and claim. Falls back to <dir> for a non-git/detached path.
_eval_sweep_canonical_root() {
    local dir="$1" common
    common="$(git -C "$dir" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" || { echo "$dir"; return; }
    [[ -n "$common" ]] || { echo "$dir"; return; }
    dirname "$common"  # <root>/.git -> <root>
}

# _eval_sweep_bounded <seconds> <cmd...>
# Run <cmd...> with a hard time bound, portably. Prefers coreutils
# gtimeout/timeout; falls back to a bash watchdog that kills the command and
# reaps its own sleep so no orphan `sleep` survives (Domain Pitfall).
_eval_sweep_bounded() {
    local secs="$1"; shift
    if command -v gtimeout >/dev/null 2>&1; then gtimeout "$secs" "$@"; return $?; fi
    if command -v timeout  >/dev/null 2>&1; then  timeout "$secs" "$@"; return $?; fi
    "$@" & local cmd_pid=$!
    ( sleep "$secs"; kill -TERM "$cmd_pid" 2>/dev/null ) & local wd_pid=$!
    wait "$cmd_pid" 2>/dev/null; local rc=$?
    # Reap the watchdog's `sleep` child before its parent, then the subshell -
    # otherwise the sleep reparents to pid 1 and becomes the orphan we are here
    # to prevent (Domain Pitfall). ponytail: pkill absent (minimal Alpine) leaves
    # the sleep, but it self-exits at $secs - a bounded orphan, and this whole
    # branch only runs when BOTH gtimeout and timeout are missing (rare).
    pkill -P "$wd_pid" 2>/dev/null
    kill "$wd_pid" 2>/dev/null; wait "$wd_pid" 2>/dev/null
    return $rc
}

# _eval_sweep_trim_log <log>
# Cap the log at the byte limit, keeping the most RECENT half (the newest run is
# the diagnosable one) rather than wiping it - a wipe at the threshold discards
# exactly the history you would reach for. Best-effort.
_eval_sweep_trim_log() {
    local log="$1" size
    [[ -f "$log" ]] || return 0
    size=$(wc -c < "$log" 2>/dev/null | tr -d ' ')
    [[ -n "$size" ]] || return 0
    if (( size > EVAL_SWEEP_LOG_MAX_BYTES )); then
        tail -c "$(( EVAL_SWEEP_LOG_MAX_BYTES / 2 ))" "$log" > "$log.tmp" 2>/dev/null \
            && mv "$log.tmp" "$log" 2>/dev/null \
            || rm -f "$log.tmp" 2>/dev/null
    fi
    return 0
}

# _eval_sweep_run_stages <repo_root> <fno_cmd> <log> [<claim_key> <holder>]
# The detached wrapper body: run each sweep/tick stage under a time bound,
# appending to <log>, then release the singleton claim. Sweep MUST precede tick
# per skill (tick consumes the run_complete the sweep emits); the wrapper bounds
# stages individually and never reorders them. Always returns 0.
_eval_sweep_run_stages() {
    local repo_root="$1" fno_cmd="$2" log="$3" claim_key="${4:-}" holder="${5:-}"
    cd "$repo_root" 2>/dev/null || return 0
    mkdir -p "$(dirname "$log")" 2>/dev/null || true
    _eval_sweep_trim_log "$log"
    printf '=== eval-sweep run %s pid=%s ===\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$$" >> "$log" 2>/dev/null || true
    _eval_sweep_bounded "$EVAL_SWEEP_STAGE_TIMEOUT" "$fno_cmd" observer sweep  --skill blueprint >> "$log" 2>&1 || true
    _eval_sweep_bounded "$EVAL_SWEEP_STAGE_TIMEOUT" "$fno_cmd" observer sweep  --skill review    >> "$log" 2>&1 || true
    _eval_sweep_bounded "$EVAL_SWEEP_STAGE_TIMEOUT" "$fno_cmd" skill-diff tick --skill blueprint >> "$log" 2>&1 || true
    _eval_sweep_bounded "$EVAL_SWEEP_STAGE_TIMEOUT" "$fno_cmd" skill-diff tick --skill review    >> "$log" 2>&1 || true
    [[ -n "$claim_key" ]] && "$fno_cmd" claim release "$claim_key" --holder "$holder" >/dev/null 2>&1
    return 0
}

# _eval_sweep_paused <fno_cmd>
# True (0) when the loops kill switch is active, so the fire is skipped. The
# eval sweep IS loop activity, so `fno loops pause-all` must stop it. Fail
# CLOSED: any output that does not begin with "not paused" / "expired" (a
# corrupt or unreadable state included) is treated as paused (Claude's
# Discretion 2 - single source of truth via the CLI, fail-closed on ambiguity).
_eval_sweep_paused() {
    local fno_cmd="$1" status
    status="$("$fno_cmd" loops status 2>/dev/null)" || return 0
    case "$status" in
        "not paused"* | "expired"*) return 1 ;;
        *) return 0 ;;
    esac
}

# _eval_sweep_try_claim <fno_cmd> <key> <holder>
# Acquire the singleton claim, classifying by `fno claim acquire`'s exit code
# (claims/cli.py): 0 = acquired (we launch), 1 = ClaimHeldByOther (a live sibling
# holds it, skip), any other = claim layer error (stale install / usage) so
# degrade to stamp-only throttling and still fire (AC1-ERR). The holder is unique
# per fire, so a still-held claim yields 1 rather than an idempotent re-acquire.
_eval_sweep_try_claim() {
    local fno_cmd="$1" key="$2" holder="$3" rc
    "$fno_cmd" claim acquire "$key" --holder "$holder" --ttl "$EVAL_SWEEP_CLAIM_TTL" >/dev/null 2>&1
    rc=$?
    case "$rc" in
        0) echo "acquired" ;;
        1) echo "held" ;;
        *) echo "degraded" ;;
    esac
}

# eval_sweep_maybe_fire <repo_root>
#
# Launches a backgrounded, detached observer sweep -> skill-diff tick for both
# pilot skills iff the daily window has elapsed, the loops are not paused, and
# no sibling worktree holds the singleton claim. Always returns 0.
eval_sweep_maybe_fire() {
    local repo_root="${1:-$PWD}"
    local canonical
    canonical="$(_eval_sweep_canonical_root "$repo_root")"
    # Only fire in an already-initialized project; never create .fno in a virgin dir.
    [[ -d "$canonical/.fno" ]] || return 0
    local stamp="$canonical/.fno/.eval-sweep-stamp"

    # Throttle: skip if the stamp is younger than the window. A missing stamp
    # (first-ever fire) is treated as "window elapsed" and fires once.
    if [[ -f "$stamp" ]]; then
        local now age
        now=$(date +%s)
        age=$(( now - $(_eval_sweep_mtime "$stamp") ))
        if (( age < EVAL_SWEEP_THROTTLE_SECONDS )); then
            return 0
        fi
    fi

    local fno_cmd
    fno_cmd="$(_reconcile_resolve_fno)" || return 0
    [[ -n "$fno_cmd" ]] || return 0

    # Honor the pause-all kill switch (fail-closed). Do NOT claim the stamp when
    # paused: an unpause should let the next session fire, not wait a full window.
    _eval_sweep_paused "$fno_cmd" && return 0

    # Singleton claim closes the burst race the stamp alone cannot (two sessions
    # starting in the same instant both saw an old stamp). Acquire BEFORE writing
    # the stamp or launching, so exactly one of a simultaneous burst proceeds.
    local slug claim_key holder claim_state
    slug="$(basename "$canonical")"
    claim_key="eval-sweep:$slug"
    holder="eval-sweep:${HOSTNAME:-h}:$$:$(date +%s)"
    claim_state="$(_eval_sweep_try_claim "$fno_cmd" "$claim_key" "$holder")"
    [[ "$claim_state" == "held" ]] && return 0
    # "acquired" -> release at wrapper end; "degraded" -> stamp-only, nothing to release.
    [[ "$claim_state" == "acquired" ]] || { claim_key=""; holder=""; }

    # Claim the throttle window (canonical stamp) so parallel callers see it fresh.
    : > "$stamp" 2>/dev/null || touch "$stamp" 2>/dev/null || true

    local log="$canonical/.fno/logs/eval-sweep.log"
    # Detach fully. All stage logic lives in _eval_sweep_run_stages (bounded,
    # logged, claim-releasing); the wrapper re-sources this lib to reach it.
    nohup bash -c '
        source "$1/eval-sweep-throttle.sh" 2>/dev/null || exit 0
        _eval_sweep_run_stages "$2" "$3" "$4" "$5" "$6"
    ' _ "$_EVAL_SWEEP_LIB_DIR" "$repo_root" "$fno_cmd" "$log" "$claim_key" "$holder" >/dev/null 2>&1 &
    disown 2>/dev/null || true

    return 0
}
