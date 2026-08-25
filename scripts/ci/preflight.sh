#!/usr/bin/env bash
# scripts/ci/preflight.sh - hermetic "CI's verdict, earlier" runner.
#
# One command to run before pushing. It validates the invoking checkout's
# committed HEAD inside a persistent, hermetic preflight worktree so a local
# green means CI green - without the canonical checkout's .fno/config.toml
# leaking into the config candidate chain (the PR-churn class this exists to
# kill). Deterministic checks only; no LLM review (that stays at config.review.*).
#
# Flow: resolve the persistent preflight worktree -> refuse a dirty or
# stale-base invoking tree -> lock -> reset the worktree to the invoking HEAD
# (caches preserved) ->
# build a hermetic env -> changed packet (CHANGED SUBSET, fail-fast, partial) ->
# fno-py doctor test smoke --keep-going -> rust-ci legs (pinned fmt, cargo test,
# advisory audit) -> one summary + exit.
#
# The changed packet is early feedback only: its failure stops the run at the
# earliest actionable signal, and its green earns nothing. Only the unchanged
# full legs can mint a mode=FULL attestation.
#
# Usage:
#   scripts/ci/preflight.sh [--retry-failed] [--force] [--wait-timeout SECS]
#     --retry-failed   re-run only the legs named in .fno/preflight-last-failed-legs.txt
#                      (a SUBSET; run a full preflight before the settle push).
#                      A missing or unparsable record runs every leg, and a run
#                      that executes every required leg earns FULL, flag or not.
#     --force          ignore a cached attestation for this SHA and run every
#                      suite. A FULL GREEN still records a fresh attestation;
#                      a RED still deletes a matching one.
#     --wait-timeout SECS   how long to queue for a held lock before giving up
#                      (default 5400 = 90m; 0 restores the old immediate
#                      fail). Waiters queue FIFO by arrival, so a waiter
#                      cannot be lapped. Do NOT wrap this script in a retry
#                      loop: the wait is built in, and N retry loops are the
#                      contention this queue exists to remove.
#
# Reuse: a FULL, non-VOID, all-legs-green run records a SHA+host-bound
# attestation beside the lock. The next caller on the same SHA + host reuses it
# and exits 0 before taking the lock, so a second caller is never blocked by a
# still-running one (a plain GREEN with no test run prints its own evidence;
# --force discards it). A RED run deletes a matching attestation.
#
# Exit codes: 0 all non-advisory suites passed; 1 a suite failed; 2 bad usage /
#   missing prerequisite; 3 lock held (immediately, or after the wait timeout);
#   4 dirty invoking tree; 5 VOID (the run lost the shared worktree or its
#   lock, so it earned no verdict - re-run; this is NOT a suite failure and
#   must not be reported as one); 6 invoking HEAD is behind origin/main
#   (rebase first - a result on a stale base cannot attest the merge head).
#
# Bash 3.2 compatible (macOS default). No flock dependency (atomic mkdir lock).

set -uo pipefail

# srm: disposable-delete helper, the sanctioned two-rung form. A PATH-wrapped
# rm trash-moves inside this script's mutexes (non-atomic work under the lock);
# command -p rm resolves via the default PATH, /bin/rm is the fallback where
# that PATH is itself shadowed. check-disposable-rm.sh fails a bare `rm` here.
# See docs/architecture/disposable-deletes.md.
srm() {
    command -p rm "$@" 2>/dev/null || /bin/rm "$@"
}

# Signal traps arm at the TOP, before any git/network child runs. Untrapped,
# a SIGINT that arrives while bash waits on a slow foreground child (git ops
# and the fetch stretch from ~1s to tens of seconds under host load) is
# swallowed after the child exits, so Ctrl-C silently does nothing. The flag
# pattern never kills mid-command; the checked points decide what exits.
LOCK_SIGNAL=0
trap 'LOCK_SIGNAL=1' INT TERM HUP

PINNED_FMT="1.94.1"   # keep in lockstep with rust-ci.yml RUSTFMT_TOOLCHAIN

RETRY_FAILED=0
FORCE_RUN=0
WAIT_TIMEOUT=5400
RECEIPT_COMMAND=("$0")
for original_arg in "$@"; do
    RECEIPT_COMMAND+=("$original_arg")
done
while [[ $# -gt 0 ]]; do
    case "$1" in
        --retry-failed) RETRY_FAILED=1 ;;
        --force) FORCE_RUN=1 ;;
        --wait-timeout)
            shift
            [[ "${1:-}" =~ ^[0-9]+$ ]] || {
                echo "preflight: --wait-timeout needs a number of seconds" >&2; exit 2; }
            WAIT_TIMEOUT="$1" ;;
        -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "preflight: unknown arg '$1'" >&2; exit 2 ;;
    esac
    shift
done

# --- resolve invoking checkout + canonical repo -----------------------------
INVOKING_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo "preflight: not a git repo" >&2; exit 2; }
COMMON_DIR="$(git rev-parse --path-format=absolute --git-common-dir)"
CANONICAL_ROOT="$(dirname "$COMMON_DIR")"
REPO_NAME="$(basename "$CANONICAL_ROOT")"

# --- resolve the persistent preflight worktree path -------------------------
# config.paths.worktrees_base if set (same knob as everything else), else the
# harness-native .claude/worktrees. Tilde-expanded.
WT_BASE="$(fno config get paths.worktrees_base 2>/dev/null | tail -1 | tr -d '[:space:]' || true)"
if [[ -n "$WT_BASE" && "$WT_BASE" != "null" && "$WT_BASE" != *Error* ]]; then
    WT_BASE="${WT_BASE/#\~/$HOME}"
    PREFLIGHT_WT="$WT_BASE/$REPO_NAME/preflight"
else
    PREFLIGHT_WT="$CANONICAL_ROOT/.claude/worktrees/preflight"
fi

# --- refuse a dirty invoking tree (AC2-ERR) ---------------------------------
DIRTY="$(git -C "$INVOKING_ROOT" status --porcelain)"
if [[ -n "$DIRTY" ]]; then
    echo "preflight: refusing - invoking worktree has uncommitted changes." >&2
    echo "preflight validates the committed HEAD; commit or stash first:" >&2
    echo "$DIRTY" | sed 's/^/  /' >&2
    exit 4
fi
CANDIDATE_SHA="$(git -C "$INVOKING_ROOT" rev-parse HEAD)"
CANDIDATE_SHORT="$(git -C "$INVOKING_ROOT" rev-parse --short HEAD)"

# --- refuse a stale base (before the cache check and the lock) ---------------
# Preflight validates the merge head: a result on a branch behind origin/main
# attests a tree that will never be merged, so waiting on (or running) it is
# wasted machine time. Checked before reuse and the lock so a stale branch
# queues for nothing. Refresh the tracking ref first (best effort, offline
# safe): a stale ref undercounts and would pass exactly the stale base this
# exists to refuse. An unresolvable origin/main is not an error here - fall
# through and run, the same fallback style as the no-merge-base packet skip.
git -C "$INVOKING_ROOT" fetch origin main --quiet 2>/dev/null || true
BEHIND_COUNT="$(git -C "$INVOKING_ROOT" rev-list --count "$CANDIDATE_SHA..origin/main" 2>/dev/null || true)"
if [[ "$BEHIND_COUNT" =~ ^[0-9]+$ ]] && (( BEHIND_COUNT > 0 )); then
    echo "preflight: refusing - HEAD is $BEHIND_COUNT commit(s) behind origin/main." >&2
    echo "preflight validates the merge head; a result on a stale base cannot attest the merged tree." >&2
    echo "rebase first: git fetch origin main && git rebase origin/main" >&2
    exit 6
fi
if ! RECEIPT_STARTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)" \
   || [[ -z "$RECEIPT_STARTED_AT" ]]; then
    echo "preflight: receipt start timestamp unavailable" >&2
    exit 1
fi
if ! RECEIPT_HOST="$(hostname 2>/dev/null)" || [[ -z "$RECEIPT_HOST" ]]; then
    echo "preflight: receipt host identity unavailable" >&2
    exit 1
fi
if ! RECEIPT_PLATFORM="$(uname -sm 2>/dev/null)" || [[ -z "$RECEIPT_PLATFORM" ]]; then
    echo "preflight: receipt platform identity unavailable" >&2
    exit 1
fi
EVENTS_PATH="$INVOKING_ROOT/.fno/events.jsonl"

# --- leg failure record (--retry-failed reads what a RED run writes here) ----
# One required scope name per line for the legs that failed the last
# verdict-bearing run; a GREEN run truncates it. This file is preflight's,
# never hand-edited, and an unreadable or unparsable file is treated as absent
# (every leg runs - the same fallback as a missing smoke failure record).
LEG_RECORD="$INVOKING_ROOT/.fno/preflight-last-failed-legs.txt"
FAILED_LEG_SCOPES=""
if [[ $RETRY_FAILED -eq 1 && -r "$LEG_RECORD" ]]; then
    while read -r _leg_line; do
        [[ -z "$_leg_line" ]] && continue
        case " smoke rustfmt:fno-agents rustfmt:fno cargo-test:fno-agents-unit cargo-test:fno-agents-e2e cargo-test:fno-unit cargo-test:fno-e2e squads-leak-guard:fno tracker-gates:fno " in
            *" $_leg_line "*)
                case " $FAILED_LEG_SCOPES " in
                    *" $_leg_line "*) ;;
                    *) FAILED_LEG_SCOPES="$FAILED_LEG_SCOPES $_leg_line" ;;
                esac ;;
        esac
    done < "$LEG_RECORD"
fi
# retry_run_leg <scope>: under --retry-failed with a usable record, run only
# the recorded legs. Any other situation (no flag, or no usable record) runs
# every leg.
retry_run_leg() {
    [[ $RETRY_FAILED -eq 0 ]] && return 0
    [[ -z "${FAILED_LEG_SCOPES// /}" ]] && return 0
    case " $FAILED_LEG_SCOPES " in *" $1 "*) return 0 ;; esac
    return 1
}
_this_host() { printf '%s\n' "$RECEIPT_HOST"; }

_json_array() {
    jq -cn --args '$ARGS.positional' -- "$@"
}

candidate_fno() {
    if [[ -x "$INVOKING_ROOT/cli/.venv/bin/python" ]]; then
        PYTHONPATH="$INVOKING_ROOT/cli/src" \
            "$INVOKING_ROOT/cli/.venv/bin/python" -m fno.cli "$@"
    elif [[ -f "$INVOKING_ROOT/cli/pyproject.toml" ]]; then
        if ! command -v uv >/dev/null 2>&1; then
            echo "preflight: candidate CLI source exists but uv is unavailable" >&2
            return 127
        fi
        PYTHONPATH="$INVOKING_ROOT/cli/src" uv run --project "$INVOKING_ROOT/cli" fno-py "$@"
    else
        fno "$@"
    fi
}

candidate_python() {
    if [[ -x "$INVOKING_ROOT/cli/.venv/bin/python" ]]; then
        PYTHONPATH="$INVOKING_ROOT/cli/src" \
            "$INVOKING_ROOT/cli/.venv/bin/python" "$@"
    elif [[ -f "$INVOKING_ROOT/cli/pyproject.toml" ]]; then
        if ! command -v uv >/dev/null 2>&1; then
            echo "preflight: candidate CLI source exists but uv is unavailable" >&2
            return 127
        fi
        PYTHONPATH="$INVOKING_ROOT/cli/src" \
            uv run --project "$INVOKING_ROOT/cli" python "$@"
    else
        python3 "$@"
    fi
}

GLOBAL_EVENTS_PATH="$(candidate_fno do pr global-receipt-events-path)" || {
    echo "preflight: canonical receipt journal path unavailable" >&2
    exit 1
}
[[ -n "$GLOBAL_EVENTS_PATH" ]] || {
    echo "preflight: canonical receipt journal path is empty" >&2
    exit 1
}

emit_verification_receipt() {
    local mode="$1" result="$2" scope_json="$3" expected="$4" executed="$5" started_at="$6" detail="$7"
    local command_json finished_at environment_json producer_json data event generation
    command_json="$(_json_array "${RECEIPT_COMMAND[@]}")" || return 1
    generation="$(next_receipt_generation)" || return 1
    finished_at="$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)" || return 1
    environment_json="$(jq -nc \
        --arg host "$RECEIPT_HOST" \
        --arg platform "$RECEIPT_PLATFORM" \
        --arg runner "scripts/ci/preflight.sh" \
        '{host:$host,platform:$platform,runner:$runner}')" || return 1
    producer_json="$(jq -nc \
        --arg kind preflight \
        --arg id "$RECEIPT_HOST:$$" \
        '{kind:$kind,id:$id}')" || return 1
    data="$(jq -nc \
        --arg candidate_sha "$CANDIDATE_SHA" \
        --argjson command "$command_json" \
        --argjson environment "$environment_json" \
        --argjson scope "$scope_json" \
        --arg started_at "$started_at" \
        --arg finished_at "$finished_at" \
        --arg mode "$mode" \
        --arg result "$result" \
        --argjson producer "$producer_json" \
        --argjson generation "$generation" \
        --argjson steps_expected "$expected" \
        --argjson steps_executed "$executed" \
        --arg detail "$detail" \
        '{candidate_sha:$candidate_sha,command:$command,environment:$environment,scope:$scope,started_at:$started_at,finished_at:$finished_at,mode:$mode,result:$result,producer:$producer,generation:$generation,steps_expected:$steps_expected,steps_executed:$steps_executed,detail:$detail}')" || return 1
    event="$(jq -nc \
        --arg ts "$finished_at" \
        --argjson data "$data" \
        '{ts:$ts,type:"verification_receipt",source:"target",data:$data}')" || return 1
    # Receipt construction stays inside this lock-holding process. There is no
    # supported CLI that can mint a trusted receipt from caller-authored fields.
    (
        # shellcheck source=scripts/lib/events-validate.sh
        source "$INVOKING_ROOT/scripts/lib/events-validate.sh"
        validate_event verification_receipt "$event"
    ) || return 1
    append_receipt_journal "$GLOBAL_EVENTS_PATH" "$event" || return 1
    if [[ "$GLOBAL_EVENTS_PATH" != "$EVENTS_PATH" ]]; then
        if ! append_receipt_journal "$EVENTS_PATH" "$event"; then
            echo "preflight: note: global receipt committed; delivery-root mirror unavailable at $EVENTS_PATH" >&2
        fi
    fi
    return 0
}

append_receipt_journal() {
    local events_path="$1" event="$2"
    candidate_python -c '
import json
import sys
from pathlib import Path
from fno.events import append_event

event = json.load(sys.stdin)
append_event(event, events_path=Path(sys.argv[1]))
' "$events_path" <<< "$event"
}

emit_setup_unavailable() {
    local reason="$1" setup_scope
    setup_scope="$(_json_array "preflight-setup")" || return 1
    emit_verification_receipt void unavailable "$setup_scope" 1 0 "$RECEIPT_STARTED_AT" "$reason"
}

next_receipt_generation() {
    candidate_fno do pr next-receipt-generation --candidate-sha "$CANDIDATE_SHA"
}

# --- attestation: reuse a prior FULL run's GREEN verdict --------------------
# A (full SHA, host) pair is a complete cache key: preflight hard-resets a
# dedicated worktree to CANDIDATE_SHA, clean -fdx's it, and scrubs the env, so
# the checked-out tree is a pure function of the SHA. A FULL GREEN records one
# attestation line in that SHA's own slot; the next caller on the same SHA +
# host reuses it. The check runs BEFORE acquire_lock so a cache hit never
# contends for the lock - that is the whole point: a second caller blocked
# behind a still-running preflight is the 'exit 3' failure this exists to remove.
# One slot per candidate SHA (keyed like the per-SHA receipt locks below): a
# shared single file let concurrently active worktrees erase each other's
# greens, degrading the hit rate to 1/N.
ATTEST_DIR="$COMMON_DIR/.preflight-attestations.d"   # sibling of .preflight.lock.d
ATTEST="$ATTEST_DIR/$CANDIDATE_SHA"
# Pull one space-separated `key=value` field out of the attestation line. Empty
# on no match, which callers treat as "not a hit" (corrupt file -> full run).
_attest_field() { printf '%s\n' "$1" | sed -n "s/.*$2=\([^ ]*\).*/\1/p"; }

reuse_attestation() {
    [[ -f "$ATTEST" ]] || return 1
    local line att_sha att_host att_pid att_at now age_s age_h
    line="$(cat "$ATTEST" 2>/dev/null)" || return 1
    [[ -n "$line" ]] || return 1
    att_sha="$(_attest_field "$line" sha)"
    [[ "$att_sha" =~ ^[0-9a-f]{40}$ ]] || return 1   # corrupt -> full run (AC2-EDGE)
    # Only a FULL green verdict satisfies the gate: a hand-edited or
    # future-different line must not pass just because its sha + host match.
    [[ "$(_attest_field "$line" mode)" == "FULL" && "$(_attest_field "$line" verdict)" == "green" ]] || return 1
    # No sha-equality check: the slot path IS the key ($ATTEST_DIR/$CANDIDATE_SHA).
    att_host="$(_attest_field "$line" host)"
    if [[ "$att_host" != "$(_this_host)" ]]; then
        echo "preflight: attestation for $CANDIDATE_SHORT rejected (foreign host: recorded=$att_host) - running full suite"
        return 1   # AC3-EDGE: a cross-environment green never satisfies the gate
    fi
    # The text file is only a fast cache carrier. Authority stays in the typed
    # event journal, so a matching carrier with missing, malformed, subset,
    # void, stale, or otherwise non-passing evidence cannot bless this SHA.
    if ! candidate_fno do pr evidence-check >/dev/null 2>&1; then
        echo "preflight: matching attestation has no exact full/passed event evidence - running full suite"
        return 1
    fi
    att_pid="$(_attest_field "$line" pid)"
    att_at="$(_attest_field "$line" at)"; att_at="${att_at//[!0-9]/}"; att_at="${att_at:-0}"
    now="$(date +%s 2>/dev/null || echo 0)"
    age_s=$((now - att_at)); (( age_s < 0 )) && age_s=0
    if   (( age_s < 60 ));   then age_h="${age_s}s"
    elif (( age_s < 3600 )); then age_h="$((age_s / 60))m"
    else age_h="$((age_s / 3600))h"; fi
    # The receipt carries its own evidence (matched SHA, age, earning pid, host):
    # a GREEN printed by a process that ran zero tests is the receipts-can-lie
    # shape, so every field is checkable and --force discards it.
    echo "preflight: GREEN (reused attestation) candidate=$CANDIDATE_SHORT earned=${age_h} ago by pid=${att_pid:-?} host=$att_host"
    echo "preflight: this verdict was earned by a FULL run on this exact SHA; --force re-runs from scratch"
    # A cancel signal arriving during the reuse check must not be swallowed
    # into a silent exit-0 GREEN now that the traps arm at the top.
    [[ "$LOCK_SIGNAL" -eq 1 ]] && exit 130
    exit 0
}

# The dirty-tree refusal above (exit 4) still wins: a cache hit must never bless
# an uncommitted tree (AC1-EDGE). --force opts out of reuse (AC2-FR).
if [[ $FORCE_RUN -eq 0 ]]; then
    reuse_attestation   # exits 0 on a hit; returns (miss) otherwise
fi

# --- lock (atomic mkdir; steal a dead holder) -------------------------------
LOCAL_LOCKDIR="$COMMON_DIR/.preflight.lock.d"
GLOBAL_LOCKDIR="$(dirname "$GLOBAL_EVENTS_PATH")/.preflight-receipt-locks/$CANDIDATE_SHA.d"
LOCAL_LOCK_ACQUIRED=0
GLOBAL_LOCK_ACQUIRED=0
LOCAL_LOCK_STAMP=""
GLOBAL_LOCK_STAMP=""
LOCKDIR="$LOCAL_LOCKDIR"
stamp_holder() {
    local stamp
    stamp="pid=$$ started=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown) host=$(hostname 2>/dev/null || echo unknown) sha=$CANDIDATE_SHA"
    printf '%s\n' "$stamp" > "$LOCKDIR/holder" || return 1
    if [[ "$LOCKDIR" == "$LOCAL_LOCKDIR" ]]; then
        LOCAL_LOCK_STAMP="$stamp"
    else
        GLOBAL_LOCK_STAMP="$stamp"
    fi
}
finish_lock_acquire() {
    if stamp_holder; then
        return 0
    fi
    # Lock-protocol deletes go through srm (never bare `rm`).
    srm -rf "$LOCKDIR"
    echo "preflight: cannot stamp lock ownership at $LOCKDIR" >&2
    exit 3
}
# --- FIFO wait queue ----------------------------------------------------------
# A bare lock plus immediate fail bred hand-rolled retry loops: every release
# was a thundering herd and an unlucky waiter was lapped indefinitely. The
# queue fixes the ordering: tickets are allocated by atomic mkdir (the same
# primitive as the lock itself), only the front ticket ever retries the real
# mkdir, and a fresh arrival that finds waiters already queued must enqueue
# rather than snipe the lock in the gap before the front waiter's next poll.
QUEUE_POLL_INTERVAL=2
TICKET=""

enqueue_ticket() {
    # Allocate ABOVE the highest surviving ticket, never in the hole a dequeued
    # front left: restarting the scan at 1 would reissue number 000001 while
    # 000002 still waits, putting a newcomer at the front of the queue.
    local n=1 candidate f
    for f in "$LOCKDIR.queue.d"/*/; do
        [[ -d "$f" ]] || continue
        # Strip the trailing slash FIRST: on a path ending in "/", ${f##*/}
        # removes the whole string and yields empty, silently skipping the
        # ticket (caught by CI on the first push of this loop).
        f="${f%/}"; f="${f##*/}"
        [[ "$f" =~ ^[0-9]+$ ]] || continue
        (( 10#$f >= n )) && n=$(( 10#$f + 1 ))
    done
    while :; do
        candidate="$LOCKDIR.queue.d/$(printf '%06d' "$n")"
        if mkdir "$candidate" 2>/dev/null; then
            printf 'pid=%s started=%s host=%s\n' "$$" \
                "$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)" \
                "$(hostname 2>/dev/null || echo unknown)" > "$candidate/holder" \
                || { srm -rf "$candidate"
                     echo "preflight: cannot stamp queue ticket at $candidate" >&2
                     exit 3; }
            TICKET="$candidate"
            return 0
        fi
        n=$((n + 1))
    done
}

dequeue_ticket() { [[ -n "$TICKET" ]] && srm -rf "$TICKET"; TICKET=""; }

ticket_is_dead() {
    # Dead = the pid is gone OR was recycled (a live but younger process now
    # owns the number; the ticket's author cannot still be it).
    local line pid
    line="$(cat "$1/holder" 2>/dev/null || echo '')"
    pid="$(printf '%s' "$line" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')"
    [[ -n "$pid" ]] && { ! kill -0 "$pid" 2>/dev/null || holder_pid_recycled "$line"; }
}

# True only for the caller whose ticket is the lowest surviving number.
# Reaps dead tickets it walks past so a crashed waiter never blocks the queue.
am_i_front() {
    local f
    for f in "$LOCKDIR.queue.d"/*/; do
        [[ -d "$f" ]] || continue
        f="${f%/}"
        if ticket_is_dead "$f"; then srm -rf "$f"; continue; fi
        [[ "$f" == "$TICKET" ]]
        return
    done
    return 1
}

queue_has_waiters() {
    local f
    for f in "$LOCKDIR.queue.d"/*/; do
        [[ -d "$f" ]] && return 0
    done
    return 1
}

holder_status_line() {
    # Augment a holder stamp with elapsed=, cpu=, and orphaned=. An unparsable
    # started= reads as now, so an odd stamp prints elapsed=0m rather than
    # aborting the status. cpu and parentage are what separate a healthy queue
    # from a starved one: the 2026-08-18 incident read "queued" for over an
    # hour while the holder sat at zero CPU with no launcher. One ps sweep per
    # printed line; the line prints at most once a minute, never per poll.
    local line="$1" started now age_s age pid cpu orphan=""
    started="$(printf '%s' "$line" | sed -n 's/.*started=\([^ ]*\).*/\1/p')"
    now="$(date +%s 2>/dev/null || echo 0)"
    age_s=$(( now - $(date -u -d "$started" +%s 2>/dev/null \
        || date -u -j -f "%Y-%m-%dT%H:%M:%SZ" "$started" +%s 2>/dev/null \
        || echo "$now") ))
    (( age_s < 0 )) && age_s=0
    if (( age_s < 3600 )); then age="$((age_s / 60))m"; else age="$((age_s / 3600))h"; fi
    pid="$(printf '%s' "$line" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')"
    # No readable pid means no measurable CPU: print ? rather than a 0 that
    # reads as a measured zero-CPU (wedged) holder. holder_tree_cpu always
    # succeeds, so there is no failure fallback to carry.
    if [[ -n "$pid" ]]; then
        cpu="cpu=$(holder_tree_cpu "$pid" 2>/dev/null)s"
    else
        cpu="cpu=?"
    fi
    holder_is_orphaned "$pid" && orphan=" orphaned=yes"
    echo "$line elapsed=$age $cpu$orphan"
}

skip_hint() {
    echo "preflight: to skip local verification and rely on CI instead of waiting, set FNO_SKIP_PREFLIGHT=1 before 'fno do pr create'/'fno do pr check' (does not apply to this script directly)" >&2
}

# --- stalled-holder liveness (progress, not existence) ------------------------
# kill -0 only sees death. The failure mode that actually wedges the fleet is a
# holder that is alive but not computing: niced under load, its suites blocked,
# the whole tree accumulating well under a second of CPU per hour. A holder
# older than STALL_MIN_AGE whose tree CPU advances less than STALL_CPU_FLOOR
# over a STALL_PROBE_SPACING window is stalled, and the front waiter steals its
# lock. The steal is safe because the victim's own exit tripwire VOIDs its
# verdict when its holder stamp no longer matches, so nothing false is ever
# reported for either side.
# Env overrides (PREFLIGHT_STALL_*) exist so tests can shrink the windows;
# non-numeric values fall back to the defaults.
STALL_MIN_AGE="${PREFLIGHT_STALL_MIN_AGE:-1200}"        # never steal a young holder (20m)
[[ "$STALL_MIN_AGE" =~ ^[0-9]+$ ]] || STALL_MIN_AGE=1200
STALL_PROBE_SPACING="${PREFLIGHT_STALL_PROBE_SPACING:-120}"   # seconds between tree-CPU samples
[[ "$STALL_PROBE_SPACING" =~ ^[0-9]+$ ]] || STALL_PROBE_SPACING=120
STALL_CPU_FLOOR="${PREFLIGHT_STALL_CPU_FLOOR:-1}"       # tree CPU seconds that count as progress
[[ "$STALL_CPU_FLOOR" =~ ^[0-9]+$ ]] || STALL_CPU_FLOOR=1

holder_tree_pids() {
    # Print the holder pid and every descendant, breadth-first.
    local pid="$1" ids="" frontier="$1" kids
    while [[ -n "$frontier" ]]; do
        kids="$(ps -eo pid=,ppid= | awk -v parents="$frontier" '
            BEGIN { n = split(parents, p, " "); for (i = 1; i <= n; i++) live[p[i]] = 1 }
            live[$2] { printf "%s ", $1 }')"
        ids="$ids $frontier"
        frontier="$kids"
    done
    echo "$ids"
}

cputime_to_s() {
    # Parse ps cputime/etime layouts ([[dd-]hh:]mm:ss with an optional .cc
    # fraction) to seconds. Every field is forced to base 10: bash reads a
    # leading zero as octal, so 08 and 09 silently abort the arithmetic and
    # zero out the sample.
    local t="$1" rest d=0 hh=0 mm=0 ss=0
    rest="${t%%.*}"
    if [[ "$rest" == *-* ]]; then d="${rest%%-*}"; rest="${rest#*-}"; fi
    case "$(printf '%s' "$rest" | awk -F: '{print NF - 1}')" in
        0) ss="$rest" ;;
        1) mm="${rest%%:*}"; ss="${rest##*:}" ;;
        *) hh="${rest%%:*}"; rest="${rest#*:}"; mm="${rest%%:*}"; ss="${rest##*:}" ;;
    esac
    echo $(( 10#$d*86400 + 10#$hh*3600 + 10#$mm*60 + 10#$ss ))
}

holder_tree_cpu() {
    # Sum accumulated CPU seconds over the holder pid and its descendants: the
    # wrapper itself idles while its suites compute, so the leaves carry the
    # progress signal. Unparsable rows read as 0, never as a steal trigger.
    local pid="$1" ids="" p total=0 t
    ids="$(holder_tree_pids "$pid")"
    for p in $ids; do
        t="$(ps -o cputime= -p "$p" 2>/dev/null | tr -d ' ')"
        [[ -n "$t" ]] || continue
        total=$(( total + $(cputime_to_s "$t") ))
    done
    echo "$total"
}

process_age_s() {
    # Age in seconds of the live process owning pid $1, from ps etime. Empty
    # output means ps cannot see the pid (caller treats as unmeasurable).
    local t
    t="$(ps -o etime= -p "$1" 2>/dev/null | tr -d ' ')"
    [[ -n "$t" ]] || return 1
    cputime_to_s "$t"
}

holder_pid_recycled() {
    # True when the pid named by a holder/ticket stamp is alive but provably
    # NOT the process that wrote the stamp: the live process is younger than
    # the stamp by more than a minute of slop. Bare kill -0 cannot tell a
    # recycled pid from the original; the confusion both wedges a queue behind
    # a phantom ticket and would signal an innocent tree on the stall path.
    local line="$1" pid started proc_age stamp_age
    pid="$(printf '%s' "$line" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')"
    [[ -n "$pid" ]] || return 1
    proc_age="$(process_age_s "$pid")" || return 1
    started="$(printf '%s' "$line" | sed -n 's/.*started=\([^ ]*\).*/\1/p')"
    stamp_age=$(( $(date +%s) - $(date -u -d "$started" +%s 2>/dev/null \
        || date -u -j -f "%Y-%m-%dT%H:%M:%SZ" "$started" +%s 2>/dev/null \
        || echo "$(date +%s)") ))
    (( stamp_age > proc_age + 60 ))
}

holder_is_orphaned() {
    # A holder reparented to pid 1 has lost its launcher: in the incident
    # shape the session that started it is gone and nothing will ever collect
    # its result, so the 20m stall floor buys nothing for it. This alone never
    # condemns: a detached holder that is computing still passes the CPU probe
    # and keeps the lock. An unreadable ppid reads as NOT orphaned, so an
    # unmeasurable holder is waited on rather than stolen.
    local ppid
    ppid="$(ps -o ppid= -p "$1" 2>/dev/null | tr -d ' ')"
    [[ "$ppid" == "1" ]]
}

path_mtime_s() {
    # A path's mtime in epoch seconds; empty when stat cannot read it. GNU
    # first: GNU stat reads -f as --file-system, so a BSD-first spelling
    # SUCCEEDS there printing prose (a line starting "File: ..."), which then
    # dies as an unbound variable inside the caller's arithmetic. BSD stat
    # rejects -c outright and falls through. The numeric guard reads any
    # surprise spelling as unmeasurable, never as a value.
    local m
    m="$(stat -c %Y "$1" 2>/dev/null || stat -f %m "$1" 2>/dev/null || true)"
    [[ "$m" =~ ^[0-9]+$ ]] && printf '%s\n' "$m" || echo ""
}

cancel_requested() {
    # Consume the one-shot cancel sentinel by atomic rename, so only one of N
    # racing waiters can win a single cancel token. Age-gated: a sentinel
    # nobody consumed (its wait already timed out or self-resolved) must never
    # cancel a later, innocent run, so past the grace it is discarded instead.
    local s="$INVOKING_ROOT/.fno/preflight-cancel" m now
    [[ -e "$s" ]] || return 1
    m="$(path_mtime_s "$s")"
    # Unmeasurable age obeys nothing and consumes nothing: fail toward waiting,
    # never toward cancelling on a token whose age cannot be checked.
    [[ -n "$m" ]] || return 1
    now="$(date +%s)"
    if (( now - m > 3600 )); then
        srm -f "$s" 2>/dev/null || true
        return 1
    fi
    mv "$s" "$s.reaped.$$" 2>/dev/null || return 1
    srm -f "$s.reaped.$$" 2>/dev/null || true
    return 0
}

lockdir_abandoned() {
    # True when an UNSTAMPED lock directory is older than the stamp grace: a
    # live holder stamps within the mkdir-to-stamp window, so an old empty
    # lockdir is a corpse that pid checks can never condemn (no pid to read).
    local m now
    m="$(path_mtime_s "$1")"
    [[ -n "$m" ]] || return 1
    now="$(date +%s)"
    (( now - m > 300 ))
}

holder_is_stalled() {
    # True when the named holder is alive, older than the age ceiling, and its
    # tree CPU has not advanced since a sample a probe window ago. Samples live
    # in globals so successive poll iterations share the baseline; a changed
    # holder line re-baselines (a new holder starts its own clock).
    local line="$1" pid started age_s now cpu_now delta
    pid="$(printf '%s' "$line" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')"
    { [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; } || return 1   # dead is the steal path, not the stall path
    started="$(printf '%s' "$line" | sed -n 's/.*started=\([^ ]*\).*/\1/p')"
    now="$(date +%s 2>/dev/null || echo 0)"
    age_s=$(( now - $(date -u -d "$started" +%s 2>/dev/null \
        || date -u -j -f "%Y-%m-%dT%H:%M:%SZ" "$started" +%s 2>/dev/null \
        || echo "$now") ))
    # The floor exists so a young holder is never condemned for a slow start.
    # An orphan has no launcher left to finish for, so the floor buys nothing
    # and costs the whole fleet twenty minutes. It still has to fail the CPU
    # probe below (LD3): detached and computing is not the same as wedged.
    if holder_is_orphaned "$pid"; then _STALL_ORPHANED=1; else _STALL_ORPHANED=0; fi
    if (( age_s < STALL_MIN_AGE && _STALL_ORPHANED == 0 )); then
        return 1
    fi
    if [[ "$line" != "${_STALL_HOLDER:-}" ]]; then
        _STALL_HOLDER="$line"; _STALL_T="$now"; _STALL_CPU="$(holder_tree_cpu "$pid")"
        return 1
    fi
    (( now - ${_STALL_T:-0} >= STALL_PROBE_SPACING )) || return 1
    cpu_now="$(holder_tree_cpu "$pid")"
    delta=$(( cpu_now - ${_STALL_CPU:-0} ))
    _STALL_T="$now"; _STALL_CPU="$cpu_now"
    # A NEGATIVE delta means a baseline child completed and left the sample:
    # that is turnover, i.e. progress, never a stall. It also covers a ps
    # hiccup dropping a row, which must fail toward waiting, not stealing.
    (( delta >= 0 && delta < STALL_CPU_FLOOR ))
}

# Rename-steal the lock from the holder named by $1 (already condemned as dead
# or stalled). Steal by rename, never `rm -rf` + `mkdir`: rename is one atomic
# operation, so exactly one of N concurrent stealers wins the corpse. With
# rm -rf, a loser deletes the lockdir the winner just recreated and both
# proceed into the one shared worktree - each then reset --hard's it mid-run
# of the other, so a suite reports pass/fail legs earned by somebody else's
# checkout.
steal_dead_lock() {
    local condemned="$1" mv_err reaped
    if mv_err="$(mv "$LOCKDIR" "$LOCKDIR.reap.$$" 2>&1)"; then
        # Steal-then-verify. Rename is atomic, but the condemnation CHECK and
        # this rename are not one operation: a racer that read the same victim
        # can be descheduled while the winner reaps it and installs its own
        # lock, then rename away that LIVE lock believing it is the corpse it
        # validated. Both then run against the one shared worktree, which is
        # the whole bug. So confirm we moved the exact holder we condemned; if
        # not, restore it and lose the race.
        reaped="$(cat "$LOCKDIR.reap.$$/holder" 2>/dev/null || echo '')"
        if [[ "$reaped" == "$condemned" ]]; then
            srm -rf "$LOCKDIR.reap.$$"
            mkdir "$LOCKDIR" 2>/dev/null && { finish_lock_acquire; return 0; }
        elif [[ -e "$LOCKDIR" ]] || ! mv "$LOCKDIR.reap.$$" "$LOCKDIR" 2>/dev/null; then
            # Someone already re-took the path. Renaming onto an existing
            # directory NESTS inside it rather than replacing it, which would
            # bury a live holder under a stray reap dir, so drop the corpse.
            srm -rf "$LOCKDIR.reap.$$"
        fi
        return 1   # lost the race: caller re-reads the live winner's stamp
    fi
    # The holder is provably condemned, so a failed reap is an environment
    # problem (permissions, read-only .git). Saying "lock held" here would send
    # the user chasing a pid they can see is not running.
    echo "preflight: cannot reap a dead holder at $LOCKDIR: $mv_err" >&2
    exit 3
}

# A stall steal must also stop the victim: the condemned tree still exists and,
# if its blocked I/O ever completes, it would resume resetting and testing the
# ONE shared preflight worktree under the stealer. Its verdict is already
# forfeit (the stamp tripwire VOIDs it), so TERM the whole tree - the victim's
# own EXIT trap then runs and releases nothing it no longer owns.
signal_holder_tree() {
    local pid="$1" p
    for p in $(holder_tree_pids "$pid"); do
        kill -TERM "$p" 2>/dev/null || true
    done
}

acquire_lock() {
    # Fast path only when nobody is queued: a fresh arrival must not snipe the
    # lock out of the gap between a release and the front waiter's next poll.
    if ! queue_has_waiters && mkdir "$LOCKDIR" 2>/dev/null; then
        finish_lock_acquire; return 0
    fi
    local holder_pid holder_line
    holder_line="$(cat "$LOCKDIR/holder" 2>/dev/null || echo '')"
    holder_pid="$(printf '%s' "$holder_line" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')"
    if ! queue_has_waiters && [[ -n "$holder_pid" ]] \
       && { ! kill -0 "$holder_pid" 2>/dev/null || holder_pid_recycled "$holder_line"; }; then
        if steal_dead_lock "$holder_line"; then return 0; fi
        # Lost the race: re-read so we name the live winner rather than the
        # corpse we just reaped.
        holder_line="$(cat "$LOCKDIR/holder" 2>/dev/null || echo '')"
    fi
    # Command negation must stay OUTSIDE [[ ]]: inside it, `! queue_has_waiters`
    # names a non-empty string and is always false, which turns an unstamped
    # lockdir into a 90-minute wait instead of this refusal.
    if [[ -z "$holder_line" ]] && ! queue_has_waiters; then
        # No parsable holder: nothing proves this lock is live, but we still
        # refuse rather than steal (a holder killed between mkdir and its stamp
        # looks identical to this). Name the path so the recovery is obvious.
        echo "preflight: lock held by an unidentified holder (no readable $LOCKDIR/holder)." >&2
        echo "preflight: if no preflight is running, remove it: rm -rf '$LOCKDIR'" >&2
        exit 3
    fi
    if [[ "$WAIT_TIMEOUT" -eq 0 ]]; then
        echo "preflight: lock held - $(holder_status_line "$holder_line")" >&2
        skip_hint
        exit 3
    fi
    mkdir -p "$LOCKDIR.queue.d" 2>/dev/null || {
        echo "preflight: cannot create the wait queue at $LOCKDIR.queue.d" >&2
        exit 3
    }
    # The give-up message advertises `touch .fno/preflight-cancel`. In a fresh
    # clone that parent is gitignored and nothing has created it, and a queued
    # waiter writes nothing under .fno, so the advertised recovery would fail
    # on a missing directory. Ensure the parent before queueing.
    mkdir -p "$INVOKING_ROOT/.fno" 2>/dev/null || true
    enqueue_ticket
    skip_hint
    local waited=0 last_print=0
    while :; do
        # Cancellation is POLLED, never signal-only: macOS bash 3.2 does not
        # run traps for INT/TERM while the shell waits on a child (verified:
        # neither a foreground sleep, a `sleep & wait`, nor a builtin read
        # delivers a pending trapped signal there; Linux bash 5 does), so a
        # signal-only cancel would ignore Ctrl-C for the whole 90m on exactly
        # the platform this fleet runs.
        if [[ "$LOCK_SIGNAL" -eq 1 ]] || cancel_requested; then
            dequeue_ticket
            echo "preflight: cancelled while queued (to cancel a wedged wait: touch $INVOKING_ROOT/.fno/preflight-cancel)" >&2
            exit 130
        fi
        if am_i_front; then
            if mkdir "$LOCKDIR" 2>/dev/null; then
                dequeue_ticket
                finish_lock_acquire
                return 0
            fi
            # Front of the queue and still blocked: the holder either runs on,
            # or is dead, recycled, abandoned-unstamped, or stalled. kill -0
            # alone cannot see a starved-but-alive holder or a recycled pid.
            holder_line="$(cat "$LOCKDIR/holder" 2>/dev/null || echo '')"
            holder_pid="$(printf '%s' "$holder_line" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')"
            if [[ -n "$holder_pid" ]] \
               && { ! kill -0 "$holder_pid" 2>/dev/null || holder_pid_recycled "$holder_line"; }; then
                if steal_dead_lock "$holder_line"; then
                    dequeue_ticket
                    echo "preflight: queue front took a dead holder's lock" >&2
                    return 0
                fi
            elif [[ -z "$holder_line" ]] && lockdir_abandoned "$LOCKDIR"; then
                # No holder file and none is coming: an unstamped corpse has no
                # pid to condemn, so age the directory itself.
                if steal_dead_lock ""; then
                    dequeue_ticket
                    echo "preflight: queue front took an abandoned unstamped lock" >&2
                    return 0
                fi
            elif holder_is_stalled "$holder_line"; then
                if steal_dead_lock "$holder_line"; then
                    dequeue_ticket
                    signal_holder_tree "$holder_pid"
                    # Which condemnation fired: an orphan steal beats the age
                    # floor, so "stalled" alone would describe a holder that
                    # never aged out. _STALL_ORPHANED holds 0 or 1; a ${var:+}
                    # expansion is wrong here because 0 is non-empty too.
                    condemn=stalled
                    (( ${_STALL_ORPHANED:-0} == 1 )) && condemn=orphaned
                    echo "preflight: EXCEPTION - stole the lock from $condemn holder pid=$holder_pid (alive but not computing; TERMed its tree; its run VOIDs)" >&2
                    return 0
                fi
            fi
        fi
        if (( waited - last_print >= 60 )); then
            holder_line="$(cat "$LOCKDIR/holder" 2>/dev/null || echo 'unknown')"
            echo "preflight: waiting (queued, ${waited}s elapsed) - $(holder_status_line "$holder_line")" >&2
            last_print=$waited
        fi
        if (( waited >= WAIT_TIMEOUT )); then
            dequeue_ticket
            echo "preflight: gave up waiting after ${WAIT_TIMEOUT}s - $holder_line" >&2
            echo "preflight: to cancel a queued wait instead of timing out: touch $INVOKING_ROOT/.fno/preflight-cancel" >&2
            exit 3
        fi
        sleep "$QUEUE_POLL_INTERVAL"
        waited=$((waited + QUEUE_POLL_INTERVAL))
    done
}
TMPHOME=""
# Release only a lock we still hold: if ours was stolen, the lockdir at this
# path now belongs to the stealer. Exact stamp matching prevents a loser from
# deleting a winner's lock during the mkdir-to-holder window.
cleanup_lock() {
    local path="$1" expected="$2" observed
    [[ -n "$path" && -n "$expected" ]] || return 0
    observed="$(cat "$path/holder" 2>/dev/null || true)"
    [[ "$observed" == "$expected" ]] && srm -rf "$path"
}
cleanup() {
    [[ -n "${TICKET:-}" ]] && srm -rf "$TICKET"
    [[ "${LOCAL_LOCK_ACQUIRED:-0}" -eq 1 ]] && cleanup_lock "${LOCAL_LOCKDIR:-}" "${LOCAL_LOCK_STAMP:-}"
    [[ "${GLOBAL_LOCK_ACQUIRED:-0}" -eq 1 ]] && cleanup_lock "${GLOBAL_LOCKDIR:-}" "${GLOBAL_LOCK_STAMP:-}"
    [[ -n "$TMPHOME" ]] && srm -rf "$TMPHOME"
}
trap cleanup EXIT
# The signal traps themselves were armed at the top of the script (see the
# comment there); this section defers ACTING on the flag only across mkdir +
# holder stamping, where exiting would leave an acquired lock without a
# complete cleanup token.
[[ "$LOCK_SIGNAL" -eq 1 ]] && exit 130
acquire_lock
LOCAL_LOCK_ACQUIRED=1
mkdir -p "$(dirname "$GLOBAL_LOCKDIR")" || {
    echo "preflight: cannot create canonical receipt lock directory" >&2
    exit 1
}
LOCKDIR="$GLOBAL_LOCKDIR"
acquire_lock
GLOBAL_LOCK_ACQUIRED=1
LOCKDIR="$LOCAL_LOCKDIR"
trap 'exit 130' INT TERM
if [[ "$LOCK_SIGNAL" -eq 1 ]]; then
    exit 130
fi

PENDING_SCOPE="$(_json_array "preflight-execution")"
if ! emit_verification_receipt void pending "$PENDING_SCOPE" 1 0 "$RECEIPT_STARTED_AT" "preflight execution started"; then
    echo "preflight: cannot persist canonical pending receipt" >&2
    exit 1
fi

# --- ensure / reset the preflight worktree ----------------------------------
echo "preflight: repo=$REPO_NAME candidate=$CANDIDATE_SHORT worktree=$PREFLIGHT_WT"
# grep, not grep -q: -q exits on first match and SIGPIPEs `git worktree list`,
# which under pipefail returns 141 (false) and would falsely recreate the wt.
is_registered() { git -C "$INVOKING_ROOT" worktree list --porcelain | grep -xF "worktree $PREFLIGHT_WT" >/dev/null; }

git -C "$INVOKING_ROOT" worktree prune >/dev/null 2>&1 || true  # drop dangling admin entries from a prior rm -rf
if is_registered; then
    : # exists and registered; reset below
elif [[ -e "$PREFLIGHT_WT" ]]; then
    echo "preflight: $PREFLIGHT_WT exists but is not a registered worktree - recreating" >&2
    srm -rf "$PREFLIGHT_WT"
    git -C "$INVOKING_ROOT" worktree prune >/dev/null 2>&1 || true
fi
if ! is_registered; then
    mkdir -p "$(dirname "$PREFLIGHT_WT")"
    git -C "$INVOKING_ROOT" worktree add --detach "$PREFLIGHT_WT" "$CANDIDATE_SHA" >/dev/null 2>&1 || {
        emit_setup_unavailable "git worktree add failed" || true
        echo "preflight: git worktree add failed" >&2; exit 1; }
fi

# Sync to candidate; keep caches. Worktrees share the object DB, so no fetch.
if ! git -C "$PREFLIGHT_WT" reset --hard "$CANDIDATE_SHA" >/dev/null 2>&1; then
    emit_setup_unavailable "git reset failed" || true
    echo "preflight: git reset --hard failed in the preflight worktree" >&2; exit 1
fi
# clean -fdx but preserve warm caches + the failure record ONLY. Excluding all
# of .fno would leave stale per-run state (e.g. triage-log.jsonl a smoke test
# reads) that could mask a regression a fresh CI checkout would catch, so we
# scope the exclusion to the single retry-record file.
git -C "$PREFLIGHT_WT" clean -fdx -e target -e cli/.venv -e .fno/preflight-last-failures.txt >/dev/null 2>&1 || {
    emit_setup_unavailable "git clean failed" || true
    echo "preflight: git clean failed in the preflight worktree" >&2; exit 1; }

# --- hermetic env ------------------------------------------------------------
REAL_HOME="$HOME"
TMPHOME="$(mktemp -d)"

# The env deliberately mirrors a fresh CI checkout: temp HOME (no ~/.fno, no
# ~/.claude, no ~/.gitconfig), FNO_* scrubbed, worktree-pinned PYTHONPATH, and
# the pytest spawn-leak guard. We intentionally do NOT pin FNO_CONFIG or
# FNO_GLOBAL_SETTINGS_PATH: pinning either one diverges from CI and breaks the
# suite's own config-fixture tests (an empty FNO_CONFIG clobbers a test's
# monkeypatched config; a /dev/null global path redirects config WRITES into
# /dev/). Two other ambient inputs a bare FNO_* scrub misses are sealed below:
#   - Ambient harness identity: preflight always runs inside a live harness, so
#     CLAUDE_CODE_SESSION_ID / CODEX_* / GEMINI_SESSION_ID are set and
#     resolve_self_model() would resolve the real session's model instead of the
#     "unknown" floor a fresh checkout produces. run_hermetic unsets every
#     HARNESS_SESSION_MARKERS name (derived from the Python single source of
#     truth, fail-closed to a literal list).
#   - Canonical config climb: a linked worktree reaches the main checkout's
#     .fno/config.toml via the shared git-common-dir (not HOME/cwd), leaking
#     worktrees_base into path/worktree tests. run_hermetic exports
#     FNO_NO_CANONICAL_CONFIG=1 so _settings_yaml_locations() drops that one
#     candidate. See docs/preflight.md.

# Derive the scrub list from AMBIENT_IDENTITY_ENV, the Python single source of
# truth for "session identity a hermetic run must not see". That constant is
# deliberately WIDER than HARNESS_SESSION_MARKERS: it also carries the legacy
# CLAUDE_SESSION_ID and the markers some modules read directly instead of through
# the resolver (CLAUDECODE_SESSION_ID in carveout/done/log, HERMES_SESSION_ID in
# the hermes adapter). Deriving from the resolver's tuple alone left every one of
# those resolving the live session. Fail closed on EITHER a nonzero exit OR empty
# output (a broken venv that prints a partial line before erroring must not slip
# past an emptiness-only check), warn, and fall back to a hardcoded literal -
# never silently skip the scrub. The fallback is a literal because it exists for
# the case where Python cannot run at all;
# cli/tests/smoke/test_preflight_hermetic.sh executes this derivation and checks
# the literal against the same constant, which is what keeps both honest.
if HARNESS_MARKERS="$(PYTHONPATH="$PREFLIGHT_WT/cli/src" python3 -c \
    'from fno.harness_identity import AMBIENT_IDENTITY_ENV; print(" ".join(AMBIENT_IDENTITY_ENV))' 2>/dev/null)" \
   && [[ -n "$HARNESS_MARKERS" ]]; then
    :
else
    echo "preflight: WARN harness-marker fetch failed; using hardcoded fallback list" >&2
    HARNESS_MARKERS="CODEX_THREAD_ID CLAUDE_CODE_SESSION_ID CODEX_SESSION_ID GEMINI_SESSION_ID OPENCODE_SESSION_ID CLAUDE_SESSION_ID CLAUDECODE CLAUDECODE_SESSION_ID HERMES_SESSION_ID TARGET_SESSION_ID CODEX_CI CODEX_INTERNAL_ORIGINATOR_OVERRIDE CODEX_SHELL CODEX_COMPANION_SESSION_ID CODEX_COMPANION_TRANSCRIPT_PATH"
fi

run_hermetic() {
    (
        cd "$PREFLIGHT_WT" || exit 1
        local v
        for v in $(compgen -v | grep '^FNO_' || true); do unset "$v"; done
        for v in $HARNESS_MARKERS; do unset "$v"; done
        export HOME="$TMPHOME"
        export FNO_THINK_SPAWN=0
        export FNO_NO_CANONICAL_CONFIG=1
        export PYTHONPATH="$PREFLIGHT_WT/cli/src"
        export CARGO_HOME="${CARGO_HOME:-$REAL_HOME/.cargo}"
        export RUSTUP_HOME="${RUSTUP_HOME:-$REAL_HOME/.rustup}"
        export UV_CACHE_DIR="${UV_CACHE_DIR:-$REAL_HOME/.cache/uv}"
        "$@"
    )
}

# --- verdict tripwire (shared by every exit path that reports a verdict) -----
# Belt-and-braces over the lock: re-verify we still own both the worktree and
# the lock before attributing a verdict to our candidate. Any residual clobber -
# a future lock bug, a hand-run `git reset` in the shared worktree - becomes a
# loud VOID instead of a GREEN or RED silently earned by another checkout.
# Compare shas only; the preflight worktree is always detached HEAD.
#
# This is a function, not a straight-line block, because the changed packet can
# exit before the full legs run: a packet that failed while the worktree was
# being reset under it earned nothing, so it must VOID rather than report RED.
exit_if_void() {
    local VOID_REASON="" WT_HEAD_NOW VOID_SCOPE_NAME="${1:-}"
    local REQUIRED_COUNT REQUIRED_SCOPE VOID_RESULT EXECUTED_COUNT
    local -a REQUIRED_SCOPE_NAMES
    # 2>/dev/null, never 2>&1: merging stderr into the value means any benign git
    # warning makes the captured string differ from the sha and VOIDs a good run.
    if ! WT_HEAD_NOW="$(git -C "$PREFLIGHT_WT" rev-parse HEAD 2>/dev/null)"; then
        VOID_REASON="cannot read the preflight worktree at $PREFLIGHT_WT"
    elif [[ "$WT_HEAD_NOW" != "$CANDIDATE_SHA" ]]; then
        VOID_REASON="worktree moved off our candidate mid-run (now ${WT_HEAD_NOW:0:12}, expected $CANDIDATE_SHORT)"
    elif [[ "$(cat "$LOCAL_LOCKDIR/holder" 2>/dev/null || true)" != "$LOCAL_LOCK_STAMP" ]]; then
        VOID_REASON="another preflight took our lock mid-run"
    elif [[ "$(cat "$GLOBAL_LOCKDIR/holder" 2>/dev/null || true)" != "$GLOBAL_LOCK_STAMP" ]]; then
        VOID_REASON="another preflight took our canonical receipt lock mid-run"
    fi
    [[ -n "$VOID_REASON" ]] || return 0
    if [[ -n "$VOID_SCOPE_NAME" ]]; then
        REQUIRED_COUNT=1
        REQUIRED_SCOPE="$(_json_array "$VOID_SCOPE_NAME")"
        EXECUTED_COUNT=1
    else
        REQUIRED_SCOPE_NAMES=(smoke rustfmt:fno-agents rustfmt:fno cargo-test:fno-agents-unit cargo-test:fno-agents-e2e cargo-test:fno-unit cargo-test:fno-e2e)
        [[ "$SQUADS_INCLUDED" -eq 1 ]] && REQUIRED_SCOPE_NAMES+=(squads-leak-guard:fno)
        # The tracker gates leg is independent of the squads guard: it runs on
        # every full pass, so it is a required scope whenever it executed
        # (riding SQUADS_INCLUDED undercounted it on machines with no real
        # squads store, and the receipt validator rejected the step mismatch).
        [[ "${TG_INCLUDED:-0}" -eq 1 ]] && REQUIRED_SCOPE_NAMES+=(tracker-gates:fno)
        REQUIRED_COUNT=${#REQUIRED_SCOPE_NAMES[@]}
        REQUIRED_SCOPE="$(_json_array "${REQUIRED_SCOPE_NAMES[@]}")"
        EXECUTED_COUNT=$REQUIRED_EXECUTED
    fi
    VOID_RESULT=unavailable
    [[ "$VOID_REASON" == worktree\ moved\ off* ]] && VOID_RESULT=stale
    emit_verification_receipt void "$VOID_RESULT" "$REQUIRED_SCOPE" "$REQUIRED_COUNT" "$EXECUTED_COUNT" "$RECEIPT_STARTED_AT" "$VOID_REASON" \
        || echo "preflight: WARN could not append VOID verification receipt" >&2
    echo "preflight: VOID - $VOID_REASON." >&2
    echo "preflight: verdict discarded - nothing here was earned by $CANDIDATE_SHORT. Re-run; this is not a code failure." >&2
    exit 5
}

# --- attestation verdict (recorded after the tripwire, below) ----------------
# VOID exited 5 above, so it reaches neither write nor delete and any
# pre-existing attestation is left untouched (AC2-ERR). A FULL GREEN records;
# any RED deletes a matching attestation so a stale green cannot outlive a real
# failure (AC3-ERR); a subset pass mints nothing (AC1-ERR: subset green is not
# full green).
record_attestation() {
    local now iso tmp
    now="$(date +%s 2>/dev/null || echo 0)"
    iso="$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)"
    mkdir -p "$ATTEST_DIR" 2>/dev/null
    tmp="${ATTEST}.$$"
    # Temp file + rename: a concurrent reader never sees a half line, and two
    # runs finishing on the same SHA make the last writer's identical content
    # harmless. Different SHAs write different slots, so a second worktree's
    # green no longer erases the first's. An unwritable common dir warns and
    # continues (AC1-ERR error boundary): the run is still green, reuse just
    # stays off until writable.
    if printf 'sha=%s mode=FULL verdict=green at=%s iso=%s host=%s pid=%s\n' \
            "$CANDIDATE_SHA" "$now" "$iso" "$(_this_host)" "$$" > "$tmp" 2>/dev/null \
       && mv -f "$tmp" "$ATTEST" 2>/dev/null; then
        # Disk bound, not a validity limit: reuse authority is the typed event
        # journal, so reaping an aged slot only costs a re-run. The mtime sweep
        # also drops temp files left by crashed writers, and the rm clears the
        # pre-per-SHA single-file carrier, which nothing reads anymore.
        find "$ATTEST_DIR" -maxdepth 1 -type f -mtime +14 -delete 2>/dev/null || true
        srm -f "$COMMON_DIR/.preflight-attestation" 2>/dev/null
    else
        srm -f "$tmp" 2>/dev/null
        echo "preflight: WARN attestation write failed ($ATTEST); reuse unavailable until writable" >&2
    fi
}
invalidate_attestation() {
    [[ -f "$ATTEST" ]] || return 0
    srm -f "$ATTEST" && echo "preflight: invalidated a stale green attestation for $CANDIDATE_SHORT"
}
# --- suites ------------------------------------------------------------------
# LEG_SCOPES holds the required-scope name(s) each leg answers to ("" = not a
# required leg: the changed packet and the advisory audit). The fmt-missing
# case names both fmt scopes on one summary line.
LEG_SCOPES=(); LEG_NAMES=(); LEG_STATUS=(); LEG_SECS=()
record_leg() { LEG_SCOPES+=("$1"); LEG_NAMES+=("$2"); LEG_STATUS+=("$3"); LEG_SECS+=("$4"); }
FAIL=0

# --- changed packet: earliest actionable signal, before the full gate --------
# Partial evidence by construction, so it runs first and stops the run on its
# own failure (a broken nearest-neighbour test should not cost a full suite to
# learn about). It can neither mint nor reuse a FULL attestation: only the
# unchanged full legs below do that. A packet that maps nothing, or cannot
# trust its diff, falls THROUGH to the full gate rather than reporting a
# verdict it did not earn. --retry-failed is a different subset mode and skips
# this leg entirely. Base/head are explicit so selection never depends on the
# preflight worktree's untracked caches or a mutable remote-tracking ref.
CHANGED_BASE=""
if [[ $RETRY_FAILED -eq 0 ]]; then
    CHANGED_BASE="$(git -C "$INVOKING_ROOT" merge-base origin/main "$CANDIDATE_SHA" 2>/dev/null || true)"
fi
if [[ -n "$CHANGED_BASE" ]]; then
    echo ""
    echo "preflight: === changed packet (CHANGED SUBSET - partial, never the gate) ==="
    c0="$SECONDS"
    run_hermetic uv run --project cli fno-py doctor test smoke --changed \
        --base "$CHANGED_BASE" --head "$CANDIDATE_SHA"
    creq=$?
    case $creq in
        0)  record_leg "" "changed packet (CHANGED SUBSET)" pass $(( SECONDS - c0 )) ;;
        20) record_leg "" "changed packet" "unselected" $(( SECONDS - c0 ))
            echo "preflight: changed packet mapped nothing - not coverage; the full gate decides" ;;
        21) record_leg "" "changed packet" "unevaluated" $(( SECONDS - c0 ))
            echo "preflight: changed packet UNEVALUATED - continuing to the full gate" ;;
        22) # The packet could not run at all. A dedicated code, not the child
            # exit-code space: several selectable lint steps exit 2 on a genuine
            # failure and pytest exits 2 on an interrupt, so keying this on 2
            # would report a real failure as a usage error and skip the gate.
            echo "preflight: changed packet could not run (missing prerequisite)" >&2
            exit 2 ;;
        *)  # Verdict-bearing exit, so it owes the same ownership check the full
            # path does: a packet that failed while the shared worktree was reset
            # under it earned nothing and must VOID rather than accuse this SHA.
            exit_if_void "changed packet (CHANGED SUBSET)"
            record_leg "" "changed packet (CHANGED SUBSET)" fail $(( SECONDS - c0 ))
            invalidate_attestation
            echo ""
            echo "preflight: SUMMARY  repo=$REPO_NAME  candidate=$CANDIDATE_SHORT  mode=CHANGED-SUBSET"
            printf '  %-24s %5ss  %s\n' fail $(( SECONDS - c0 )) "changed packet (CHANGED SUBSET)"
            echo ""
            echo "preflight: RED (changed packet) - stopped at the earliest signal; the full gate has NOT run." >&2
            echo "preflight: fix, commit, then re-run scripts/ci/preflight.sh" >&2
            exit 1 ;;
    esac
elif [[ $RETRY_FAILED -eq 0 ]]; then
    echo "preflight: no merge-base with origin/main - skipping the changed packet"
fi

echo ""
REQUIRED_EXECUTED=0
SQUADS_INCLUDED=0
TG_INCLUDED=0
RECEIPT_UNAVAILABLE=0
if retry_run_leg smoke; then
    echo "preflight: === smoke suite ($([[ $RETRY_FAILED -eq 1 ]] && echo retry-failed || echo keep-going)) ==="
    SMOKE_ARGS=(--keep-going); [[ $RETRY_FAILED -eq 1 ]] && SMOKE_ARGS=(--retry-failed --keep-going)
    s0="$SECONDS"
    # Bootstrap via `uv run --project cli`: there is no repo-root pyproject and
    # no global fno-py in a hermetic env, so a bare `fno doctor test smoke` is not on
    # PATH until uv syncs the cli project (uv auto-syncs). The attestation
    # logic below is unchanged: a FULL GREEN records, a RED deletes, a subset
    # mints nothing.
    # The smoke registry auto-discovers scripts/tests/stress-rust-e2e-concurrency.sh
    # and its default is 20 trials, which is 12 minutes by its own measured
    # 35.9s/trial. Preflight already runs those same binaries serialized in the
    # cargo-test e2e legs below, so the full denominator here is duplication on
    # a pre-push gate. One trial proves the harness still runs; rust-ci owns the
    # 20.
    run_hermetic env STRESS_TRIALS=1 uv run --project cli fno-py doctor test smoke "${SMOKE_ARGS[@]}"
    sreq=$?
    REQUIRED_EXECUTED=$((REQUIRED_EXECUTED + 1))
    [[ $sreq -eq 0 ]] && record_leg smoke "smoke suite" pass $(( SECONDS - s0 )) || { record_leg smoke "smoke suite" fail $(( SECONDS - s0 )); FAIL=1; }
else
    echo "preflight: === smoke suite (skipped - not in the retry leg record) ==="
    record_leg "" "smoke suite" skipped 0
fi

# tracker gates (zero-overlap partition + consumer census) -------------------
# Static, seconds-fast, the same instruments CI runs
# (.github/workflows/tracker-partition.yml): the sidecar/tracker partition
# with its self-test, and the graph-consumer census (verbs + reads) with its
# self-test, on the hermetic candidate tree.
if retry_run_leg tracker-gates:fno; then
    echo "preflight: === tracker gates (partition + consumer census) ==="
    tg0="$SECONDS"
    run_hermetic bash scripts/ci/check-tracker-partition.sh --self-test
    tgp=$?
    run_hermetic bash scripts/ci/check-tracker-partition.sh
    tgp2=$?
    [[ $tgp2 -gt $tgp ]] && tgp=$tgp2
    run_hermetic bash scripts/ci/check-tracker-consumers.sh
    tgc=$?
    REQUIRED_EXECUTED=$((REQUIRED_EXECUTED + 1))
    TG_INCLUDED=1
    if [[ $tgp -eq 0 && $tgc -eq 0 ]]; then
        record_leg tracker-gates:fno "tracker gates (fno)" pass $(( SECONDS - tg0 ))
    else
        record_leg tracker-gates:fno "tracker gates (fno)" fail $(( SECONDS - tg0 ))
        FAIL=1
    fi
else
    echo "preflight: === tracker gates (skipped - not in the retry leg record) ==="
    record_leg "" "tracker gates (fno)" skipped 0
fi

# rust-ci legs (pinned fmt, cargo test, advisory audit) ----------------------
have_pinned_fmt() { rustup toolchain list 2>/dev/null | grep "^$PINNED_FMT" >/dev/null; }

FNO_AGENTS_INTEGRATION_TARGETS="--test active_backlog_drain --test agy_ask_unit --test claude_ask_dispatch --test claude_ask_parity --test claude_bg_create --test codex_ask_dispatch --test codex_ask_parity --test codex_ask_sigint --test codex_ask_unit --test daemon_e2e --test finalize_e2e --test flock_interop --test gemini_ask_sigint --test gemini_ask_unit --test generic_completion --test kill_criteria_parity --test loop_check --test loop_runtime --test loop_target --test loopcheck_decisions --test loopcheck_hook_payload --test loopcheck_missing_manifest --test opencode_serve_journey --test provider_contract --test review_coverage_paths --test review_coverage_verb --test run_outcome --test run_state --test spawn_routing --test verify_evidence_parity"
FNO_INTEGRATION_TARGETS="--test agent_edge_e2e --test client_e2e --test idle_reaper_e2e --test keymap_e2e --test layout_e2e --test mouse_e2e --test multiclient_e2e --test mux_config_e2e --test persistence --test proto_socket --test script_api_e2e --test search_e2e --test server_spine --test squad_prune --test squad_prune_live_pane --test workspace_persistence_e2e"

run_rust_leg() { # scope  name  cwd  cmd...
    local scope="$1" name="$2" cwd="$3"; shift 3
    echo ""
    echo "preflight: === $name ==="
    local t0="$SECONDS"
    run_hermetic bash -c "cd '$cwd' && $*"
    local rc=$?
    if [[ $rc -eq 0 ]]; then record_leg "$scope" "$name" pass $(( SECONDS - t0 ))
    else record_leg "$scope" "$name" fail $(( SECONDS - t0 )); FAIL=1; fi
}

skip_rust_leg() { # scope  name
    echo "preflight: === $2 (skipped - not in the retry leg record) ==="
    record_leg "" "$2" skipped 0
}

if have_pinned_fmt; then
    if retry_run_leg rustfmt:fno-agents; then
        run_rust_leg rustfmt:fno-agents "cargo fmt --check (fno-agents, +$PINNED_FMT)" "crates/fno-agents" "cargo +$PINNED_FMT fmt --all --check"
        REQUIRED_EXECUTED=$((REQUIRED_EXECUTED + 1))
    else
        skip_rust_leg rustfmt:fno-agents "cargo fmt --check (fno-agents, +$PINNED_FMT)"
    fi
    if retry_run_leg rustfmt:fno; then
        run_rust_leg rustfmt:fno "cargo fmt --check (fno, +$PINNED_FMT)" "crates/fno" "cargo +$PINNED_FMT fmt --all --check"
        REQUIRED_EXECUTED=$((REQUIRED_EXECUTED + 1))
    else
        skip_rust_leg rustfmt:fno "cargo fmt --check (fno, +$PINNED_FMT)"
    fi
else
    echo "preflight: pinned rustfmt toolchain $PINNED_FMT not installed - fmt leg cannot match rust-ci" >&2
    echo "preflight: install it: rustup toolchain install $PINNED_FMT --component rustfmt" >&2
    record_leg "rustfmt:fno-agents rustfmt:fno" "cargo fmt --check (+$PINNED_FMT MISSING)" fail 0; FAIL=1
fi

if retry_run_leg cargo-test:fno-agents-unit; then
    run_rust_leg cargo-test:fno-agents-unit "cargo test --lib --bins (fno-agents)" "crates/fno-agents" "cargo test --lib --bins"
    REQUIRED_EXECUTED=$((REQUIRED_EXECUTED + 1))
else
    skip_rust_leg cargo-test:fno-agents-unit "cargo test --lib --bins (fno-agents)"
fi
if retry_run_leg cargo-test:fno-agents-e2e; then
    run_rust_leg cargo-test:fno-agents-e2e "cargo test explicit integration targets --test-threads=1 (fno-agents)" "crates/fno-agents" "cargo test $FNO_AGENTS_INTEGRATION_TARGETS -- --test-threads=1"
    REQUIRED_EXECUTED=$((REQUIRED_EXECUTED + 1))
else
    skip_rust_leg cargo-test:fno-agents-e2e "cargo test explicit integration targets --test-threads=1 (fno-agents)"
fi

# squads.json leak guard (x-e447 US3): snapshot the REAL store mtime around the
# crates/fno real-process integration leg. A test that bypasses run_hermetic's HOME redirect and
# writes the real ~/.fno/squads.json would otherwise stay green; this is the
# class-level assertion (PR #589's assert_writable closed the build-tree binary
# arm; this catches every other path at once). Read-only, stdlib, degrade-to-skip
# only when absent; an unavailable stat fails closed. A concurrent real mux
# session is a valid writer, so the message names that caveat rather than
# asserting exclusive ownership.
_real_squads_state() {
    python3 -c "
import os, re
# The store follows the mux state root (state_dir off the global config), so
# guard BOTH the legacy location and the resolved one: a machine with state_dir
# configured no longer touches ~/.fno/squads.json, and a guard watching only
# the legacy file reads vacuously green there. One token out: 'present:<all
# mtimes>' when any copy exists, 'absent' when none does, 'unavailable' when
# any stat cannot answer (fail closed).
roots = [os.path.expanduser('~/.fno')]
cfg = os.path.expanduser('~/.fno/config.toml')
try:
    body = open(cfg).read()
    # Every state_dir occurrence, top-level AND [config]-wrapped (the mirror
    # reads both, wrapped winning): watching an extra root is free, missing a
    # relocated one is the vacuous green this guard exists to prevent.
    # Template and env-var-reference values stay skipped: the resolver
    # declines those too, so the store does not follow them.
    vals = re.findall(r'^\s*state_dir\s*=\s*[\"\\']([^\"\x27]+)[\"\\']', body, re.M)
    # A TOML multiline string is a legal spelling of the same key. The
    # quoted pattern cannot see it, and an unwatched relocated root is the
    # vacuous green this guard exists to prevent.
    vals += re.findall(r'state_dir\s*=\s*[\"\\']{3}\s*\n\s*([^\"\x27\n]+)', body)
    for v in vals:
        if not v.startswith(('{', '\$')):
            roots.append(os.path.abspath(os.path.expanduser(v)))
except OSError:
    pass
states = []
for r in roots:
    p = os.path.join(r, 'squads.json')
    try:
        states.append(f'present:{os.stat(p).st_mtime_ns}')
    except FileNotFoundError:
        states.append('absent')
    except OSError:
        states.append('unavailable')
if any(s == 'unavailable' for s in states):
    print('unavailable')
elif any(s.startswith('present:') for s in states):
    print('present:' + ','.join(s for s in states if s.startswith('present:')))
else:
    print('absent')
" 2>/dev/null
}
# The leak guard snapshots the real store around BOTH crates/fno test legs, so
# the trio runs together: selecting either scope runs both legs.
RUN_FNO_UNIT=0
RUN_FNO_E2E=0
RUN_FNO_SQUADS=0
retry_run_leg cargo-test:fno-unit && RUN_FNO_UNIT=1
retry_run_leg cargo-test:fno-e2e && RUN_FNO_E2E=1
retry_run_leg squads-leak-guard:fno && RUN_FNO_SQUADS=1
[[ $RUN_FNO_SQUADS -eq 1 ]] && RUN_FNO_E2E=1

# Sampled BEFORE the unit leg, not between the two. The guard asserts a class
# ("no crates/fno test writes the real squads.json"), and the leg split put
# `cargo test --lib --bins` ahead of it: a lib test that bypasses the HOME
# redirect ran outside the mtime window entirely and the guard still recorded a
# pass. The window has to open before the first test in the pair runs.
if [[ $RUN_FNO_SQUADS -eq 1 ]]; then
    _squads_before="$(_real_squads_state || printf '%s' unavailable)"
    case "$_squads_before" in
        absent|unavailable|present:*) ;;
        *) _squads_before=unavailable ;;
    esac
fi

if [[ $RUN_FNO_UNIT -eq 1 ]]; then
    run_rust_leg cargo-test:fno-unit "cargo test --lib --bins (fno)" "crates/fno" "cargo test --lib --bins"
    REQUIRED_EXECUTED=$((REQUIRED_EXECUTED + 1))
else
    skip_rust_leg cargo-test:fno-unit "cargo test --lib --bins (fno)"
fi

if [[ $RUN_FNO_E2E -eq 1 ]]; then
    run_rust_leg cargo-test:fno-e2e "cargo test explicit integration targets --test-threads=1 (fno)" "crates/fno" "cargo test $FNO_INTEGRATION_TARGETS -- --test-threads=1"
    REQUIRED_EXECUTED=$((REQUIRED_EXECUTED + 1))
    if [[ $RUN_FNO_SQUADS -eq 1 ]]; then
        _squads_after="$(_real_squads_state || printf '%s' unavailable)"
        case "$_squads_after" in
            absent|unavailable|present:*) ;;
            *) _squads_after=unavailable ;;
        esac
        SQUADS_INCLUDED=1
        if [[ "$_squads_before" == "absent" && "$_squads_after" == "absent" ]]; then
            SQUADS_INCLUDED=0
            record_leg "" "squads.json leak guard (fno)" "not configured (no real store)" 0
            SQUADS_SCOPE="$(_json_array squads-leak-guard:fno)"
            emit_verification_receipt advisory not_configured "$SQUADS_SCOPE" 1 0 "$RECEIPT_STARTED_AT" "no real squads store" \
                || echo "preflight: WARN could not append squads not-configured receipt" >&2
        elif [[ "$_squads_before" == "unavailable" || "$_squads_after" == "unavailable" ]]; then
            REQUIRED_EXECUTED=$((REQUIRED_EXECUTED + 1))
            RECEIPT_UNAVAILABLE=1
            FAIL=1
            record_leg squads-leak-guard:fno "squads.json leak guard (fno)" unavailable 0
        elif [[ "$_squads_after" != "$_squads_before" ]]; then
            REQUIRED_EXECUTED=$((REQUIRED_EXECUTED + 1))
            echo "preflight: FAIL real ~/.fno/squads.json changed during crates/fno explicit integration tests" \
                 "(mtime $_squads_before -> $_squads_after)" >&2
            echo "  if a real mux session is running concurrently it is a valid writer;" >&2
            echo "  otherwise a crates/fno doctor test bypassed the HOME redirect and leaked." >&2
            FAIL=1
            record_leg squads-leak-guard:fno "squads.json leak guard (fno)" fail 0
        else
            REQUIRED_EXECUTED=$((REQUIRED_EXECUTED + 1))
            record_leg squads-leak-guard:fno "squads.json leak guard (fno)" pass 0
        fi
    fi
else
    skip_rust_leg cargo-test:fno-e2e "cargo test explicit integration targets --test-threads=1 (fno)"
fi

# advisory: never flips the exit code
echo ""
echo "preflight: === cargo audit (ADVISORY) ==="
ADVISORY_STARTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || true)"
ADVISORY_EXECUTED=0
if run_hermetic bash -c "command -v cargo-audit >/dev/null 2>&1"; then
    a0="$SECONDS"
    ADVISORY_FAILED=0
    run_hermetic bash -c "cd crates/fno-agents && cargo audit" || ADVISORY_FAILED=1
    ADVISORY_EXECUTED=$((ADVISORY_EXECUTED + 1))
    run_hermetic bash -c "cd crates/fno && cargo audit" || ADVISORY_FAILED=1
    ADVISORY_EXECUTED=$((ADVISORY_EXECUTED + 1))
    if [[ $ADVISORY_FAILED -eq 0 ]]; then
        record_leg "" "cargo audit (ADVISORY)" pass $(( SECONDS - a0 ))
    else
        record_leg "" "cargo audit (ADVISORY)" "advisory-fail" $(( SECONDS - a0 ))
    fi
else
    record_leg "" "cargo audit (ADVISORY)" "skipped (not installed)" 0
fi
ADVISORY_STATUS="${LEG_STATUS[${#LEG_STATUS[@]}-1]}"
case "$ADVISORY_STATUS" in
    pass) ADVISORY_RESULT=passed ;;
    advisory-fail) ADVISORY_RESULT=failed ;;
    *) ADVISORY_RESULT=unavailable ;;
esac
ADVISORY_SCOPE="$(_json_array "cargo-audit:fno-agents" "cargo-audit:fno")"
if [[ -z "$ADVISORY_STARTED_AT" ]]; then
    echo "preflight: WARN advisory receipt timestamp unavailable" >&2
elif ! emit_verification_receipt advisory "$ADVISORY_RESULT" "$ADVISORY_SCOPE" 2 "$ADVISORY_EXECUTED" "$ADVISORY_STARTED_AT" "$ADVISORY_STATUS"; then
    echo "preflight: WARN could not append advisory verification receipt" >&2
fi

# --- verdict tripwire --------------------------------------------------------
# Belt-and-braces over the lock: re-verify we still own both the worktree and
# the lock before attributing a verdict to our candidate. Any residual clobber -
# a future lock bug, a hand-run `git reset` in the shared worktree - becomes a
# loud VOID instead of a GREEN or RED silently earned by another checkout.
# Compare shas only; the preflight worktree is always detached HEAD.
exit_if_void

if [[ $RETRY_FAILED -eq 0 && $FAIL -eq 0 ]]; then
    : # receipt first; the carrier is recorded only after evidence is durable
elif [[ $FAIL -ne 0 ]]; then
    invalidate_attestation
fi

# Refresh the leg failure record from THIS run: a RED names the required legs
# that did not pass, a GREEN truncates. Skipped legs record nothing (they did
# not run, so they can neither pass nor fail here). Temp file + rename so a
# concurrent reader never sees a half-written record; an unwritable path warns
# and leaves --retry-failed on its run-every-leg fallback.
write_leg_record() {
    local tmp="$LEG_RECORD.$$" failed="" i scope
    for i in "${!LEG_SCOPES[@]}"; do
        [[ -n "${LEG_SCOPES[$i]}" ]] || continue
        case "${LEG_STATUS[$i]}" in pass|skipped) continue ;; esac
        for scope in ${LEG_SCOPES[$i]}; do
            case " $failed " in
                *" $scope "*) ;;
                *) failed="$failed $scope" ;;
            esac
        done
    done
    if ! mkdir -p "$(dirname "$LEG_RECORD")" 2>/dev/null; then
        echo "preflight: WARN cannot create leg-record dir; --retry-failed will run every leg" >&2
        return 0
    fi
    : > "$tmp"
    [[ -n "$failed" ]] && printf '%s\n' $failed >> "$tmp"
    if mv -f "$tmp" "$LEG_RECORD" 2>/dev/null; then
        return 0
    fi
    srm -f "$tmp" 2>/dev/null
    echo "preflight: WARN leg-record write failed; --retry-failed will run every leg" >&2
}
write_leg_record

REQUIRED_SCOPE_NAMES=(smoke rustfmt:fno-agents rustfmt:fno cargo-test:fno-agents-unit cargo-test:fno-agents-e2e cargo-test:fno-unit cargo-test:fno-e2e)
[[ "$SQUADS_INCLUDED" -eq 1 ]] && REQUIRED_SCOPE_NAMES+=(squads-leak-guard:fno)
[[ "${TG_INCLUDED:-0}" -eq 1 ]] && REQUIRED_SCOPE_NAMES+=(tracker-gates:fno)
REQUIRED_COUNT=${#REQUIRED_SCOPE_NAMES[@]}
REQUIRED_SCOPE="$(_json_array "${REQUIRED_SCOPE_NAMES[@]}")"
# Coverage-derived mode: a run that executed every required leg is FULL whether
# or not --retry-failed was typed; only a run that skipped legs is a SUBSET.
# This is what makes the no-record retry fallback usable: it does the whole
# job and mints the receipt it earned.
RECEIPT_MODE=full
[[ $REQUIRED_EXECUTED -lt $REQUIRED_COUNT ]] && RECEIPT_MODE=subset
RECEIPT_RESULT=passed
[[ $FAIL -ne 0 ]] && RECEIPT_RESULT=failed
[[ $RECEIPT_UNAVAILABLE -eq 1 ]] && RECEIPT_RESULT=unavailable
if ! emit_verification_receipt "$RECEIPT_MODE" "$RECEIPT_RESULT" "$REQUIRED_SCOPE" "$REQUIRED_COUNT" "$REQUIRED_EXECUTED" "$RECEIPT_STARTED_AT" "preflight suite verdict"; then
    echo "preflight: verification receipt append failed; verdict is unavailable" >&2
    FAIL=1
    invalidate_attestation
elif [[ "$RECEIPT_MODE" == "full" && $FAIL -eq 0 ]]; then
    record_attestation
fi

# --- summary -----------------------------------------------------------------
echo ""
echo "preflight: SUMMARY  repo=$REPO_NAME  candidate=$CANDIDATE_SHORT  mode=$([[ "$RECEIPT_MODE" == subset ]] && echo RETRY-SUBSET || echo FULL)"
[[ "$RECEIPT_MODE" == subset ]] && echo "preflight: RETRY SUBSET - run a full preflight before the settle-green push"
for i in "${!LEG_NAMES[@]}"; do
    printf '  %-24s %5ss  %s\n' "${LEG_STATUS[$i]}" "${LEG_SECS[$i]}" "${LEG_NAMES[$i]}"
done
echo ""
if [[ $FAIL -eq 0 ]]; then
    if [[ "$RECEIPT_MODE" == "full" && -f "$ATTEST" ]]; then
        echo "preflight: GREEN - safe to push $CANDIDATE_SHORT (attestation recorded; next call on this SHA reuses it)"
    else
        echo "preflight: GREEN - safe to push $CANDIDATE_SHORT"
    fi
    exit 0
else
    echo "preflight: RED - fix, commit, then 'scripts/ci/preflight.sh --retry-failed'" >&2
    exit 1
fi
