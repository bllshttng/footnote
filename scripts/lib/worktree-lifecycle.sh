#!/usr/bin/env bash
# Worktree lifecycle management
# Usage:
#   worktree-lifecycle.sh status                    # List all worktrees
#   worktree-lifecycle.sh cleanup [--older-than Nd] [--dry-run] [--prefix <prefix>]
#   worktree-lifecycle.sh cleanup --merged [--apply] [--kill-orphans]
#   worktree-lifecycle.sh archive <name>            # Keep branch, remove directory
set -uo pipefail

# The one "is removing this worktree safe?" answer, shared with
# archive-worktree.sh and the Rust row-GC probe. Absent (partial deploy) it
# degrades to the old block-on-any-dirt rule, never to permission.
_WT_LIFECYCLE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ -f "${_WT_LIFECYCLE_DIR}/worktree-reapable.sh" ]]; then
    # shellcheck source=/dev/null
    source "${_WT_LIFECYCLE_DIR}/worktree-reapable.sh"
else
    WT_REAPABLE_LINE=""
    wt_reapable() {
        # The receipt has to match the answer, or the reason printed below lies:
        # the old shape stamped "probe-failed" even on the path that returned 0.
        # A git that ERRORS also blocks here; an empty porcelain from a failed
        # command is not a clean tree.
        local out rc=0 reason=dirty
        out="$(git -C "${1:-}" status --porcelain 2>/dev/null)" || rc=$?
        if [[ "$rc" -eq 0 && -z "$out" ]]; then
            WT_REAPABLE_LINE="reapable=yes reason=clean recoverable_deletions=0"
            return 0
        fi
        [[ "$rc" -ne 0 ]] && reason=probe-failed
        WT_REAPABLE_LINE="reapable=no reason=${reason} detail=helper-missing"
        return 1
    }
fi

# The detached-HEAD reachability answer, shared with archive-worktree.sh. A
# partial deploy that dropped the lib keeps everything (count 1), never reaps;
# its refresh stub keeps today's fetch-what-the-merged-check-needs behavior.
if [[ -f "${_WT_LIFECYCLE_DIR}/worktree-unpushed.sh" ]]; then
    # shellcheck source=/dev/null
    source "${_WT_LIFECYCLE_DIR}/worktree-unpushed.sh"
else
    wt_unpushed_count() { printf '1\n'; }
    wt_refresh_remote_refs() { git -C "${1:-.}" fetch origin main >/dev/null 2>&1; }
fi

# --- merged-mode helpers (used only by `cleanup --merged`) ------------------

# Live target session? Legacy manifests carried status: IN_PROGRESS; the modern
# immutable manifest has no status field, so the durable signal is the node
# claim (session-pid anchored + TTL). owner_pid is checked last and only as a
# positive signal: it is the transient `fno do target init` wrapper pid, dead about
# a second after init returns, so on its own this returned 1 for every live
# session and the merged-cleanup sweep would prune a running target's worktree.
_wt_live() {
    local st="$1/.fno/target-state.md"
    [[ -f "$st" ]] || return 1
    grep -qE '^status:[[:space:]]*IN_PROGRESS' "$st" && return 0
    local guard_lib
    guard_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/target-guard.sh"
    # shellcheck source=./target-guard.sh
    if [[ -f "$guard_lib" ]] && source "$guard_lib" 2>/dev/null; then
        target_claim_is_live "$st" && return 0
    fi
    local pid
    # Pipeline-free extraction so a no-match never SIGPIPEs an upstream grep.
    pid="$(sed -nE '/^owner_pid:[[:space:]]*[0-9]+/{s/^owner_pid:[[:space:]]*//;p;q;}' "$st" 2>/dev/null)"
    [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null && return 0
    return 1
}

_wt_app_owned() {
    local wt="$1" raw root
    raw="${CODEX_HOME:-$HOME/.codex}/worktrees"
    [[ -d "$raw" ]] || return 1
    root="$(cd "$raw" 2>/dev/null && pwd)" || return 1
    case "$wt/" in
        "$root/"*) return 0 ;;
        *) return 1 ;;
    esac
}

# Permanent by design: scripts/ci/preflight.sh pins a scratch worktree named
# `preflight` (hard-reset to the candidate SHA per run, caches deliberately
# preserved); hermeticity comes from the reset, not from disposal. One
# predicate for BOTH removal paths so a second permanent tree is a one-line
# change here, not two loop edits 125 lines apart. See
# docs/state-root-inventory.md for the recorded entry.
_wt_permanent() {
    [[ "$(basename "$1")" == "preflight" ]]
}

# PIDs actually rooted in the worktree (cwd under it) OR whose cmdline
# references it. Mirrors archive-worktree.sh's enumeration (escaped regex so
# path metachars are literal); drops our own PID and our own tooling.
#
# The shared lsof snapshot contains cwd descriptors only, so uv-hardlinked venv
# `.so` files mmapped by long-lived daemons never count as rooted here. The
# pgrep lane still catches background processes carrying the path in argv.
_WT_CWD_SNAPSHOT=""
_WT_CWD_SNAPSHOT_OK=0

_wt_refresh_cwd_snapshot() {
    local raw=""
    _WT_CWD_SNAPSHOT=""
    _WT_CWD_SNAPSHOT_OK=0
    command -v lsof >/dev/null 2>&1 || return 1
    raw="$(lsof -a -d cwd -Fpn 2>/dev/null)" || return 1
    _WT_CWD_SNAPSHOT="$(printf '%s\n' "$raw" | awk '
        /^p[0-9]+$/ { pid = substr($0, 2); next }
        /^n/ && pid != "" { print pid "\t" substr($0, 2) }
    ')"
    _WT_CWD_SNAPSHOT_OK=1
}

_wt_pids() {
    local wt="$1" root pids="" pids_f="" re candidates filtered snapshot_rc=0
    root="$(cd "$wt" 2>/dev/null && pwd -P)" || root="$wt"
    if [[ "${_WT_CWD_SNAPSHOT_OK:-0}" -eq 1 ]]; then
        pids="$(printf '%s\n' "${_WT_CWD_SNAPSHOT:-}" | awk -F '\t' -v root="$root" -v logical="$wt" '
            $2 == root || index($2, root "/") == 1 ||
            $2 == logical || index($2, logical "/") == 1 { print $1 }
        ' | sort -u)"
    else
        snapshot_rc=2
    fi
    re="$(printf '%s' "$wt" | sed -e 's/[][\\.^$*+?(){}|/]/\\&/g')"
    pids_f="$(pgrep -f -- "$re" 2>/dev/null || true)"
    # Drop our own PID: a concurrent sweep carries the worktree path in its
    # argv (a different PGID, so pgrep -f matches it too), and its own
    # machinery is filtered below, never treated as a squatter.
    # `|| true`: a mid-pipeline `grep -v` with no match exits 1, which pipefail
    # would surface as the function's status even though the pids printed fine.
    candidates="$(printf '%s\n%s\n' "$pids" "$pids_f" | grep -v "^$$\$" | grep -v '^$' | sort -u || true)"
    if [[ -z "$candidates" ]]; then
        return "$snapshot_rc"
    fi
    # One process-table snapshot for the whole candidate set, not one `ps`
    # subprocess per pid: a concurrent sweep's own argv carries every
    # worktree path (see the lock comment above), so candidates scale with
    # the number of overlapping sweeps and a per-pid `ps` turned that into
    # N sweeps x 49 worktrees x N matches.
    # The marker keeps awk's first input non-empty and positively identifies a
    # completed snapshot; otherwise an empty ps makes the candidates FNR==NR.
    filtered="$(awk '
        BEGIN { snapshot_marker = "__FNO_PS_SNAPSHOT_COMPLETE__" }
        FNR==NR {
            line = $0
            if (line == snapshot_marker) {
                snapshot_complete = 1
                next
            }
            snapshot_rows++
            sub(/^[ \t]+/, "", line)
            pid = $1
            sub("^" pid "[ \t]+", "", line)
            cmdbypid[pid] = line
            next
        }
        {
            pid = $1
            if (!snapshot_complete || snapshot_rows == 0) {
                print pid
                next
            }
            cmd = (pid in cmdbypid) ? cmdbypid[pid] : ""
            if (cmd ~ /archive-worktree\.sh/ || cmd ~ /worktree-lifecycle\.sh/) next
            print pid
        }
    ' <(ps -Ao pid=,command= 2>/dev/null; printf '%s\n' '__FNO_PS_SNAPSHOT_COMPLETE__') <(printf '%s\n' "$candidates"))"
    printf '%s\n' "$filtered"
    return "$snapshot_rc"
}

# Print bg-job ids (~/.claude/jobs/<id>/) safe to retire: state in
# done/stopped/failed, cwd matching the selector, cwd NOT the canonical
# checkout. Selector is either an exact worktree path or the literal
# "__MISSING__" (cwd no longer exists on disk - the final-pass mode). This
# closes the pr-watch loop: the sweep that archives a pr-merged-<n> worktree
# is the same sweep that retires its now-dangling job record.
_reap_job_candidates() {
    local selector="$1" canonical="$2"
    command -v python3 >/dev/null 2>&1 || return 0
    python3 - "$selector" "$canonical" <<'PY' 2>/dev/null
import glob, json, os, sys
selector, canonical = sys.argv[1], sys.argv[2]
DEAD = {"done", "stopped", "failed"}
canon = os.path.abspath(canonical) if canonical else ""
for sj in glob.glob(os.path.expanduser("~/.claude/jobs/*/state.json")):
    try:
        with open(sj) as f:
            d = json.load(f)
    except Exception:
        continue
    if d.get("state") not in DEAD:
        continue
    cwd = d.get("cwd") or ""
    if not cwd:
        continue
    acwd = os.path.abspath(cwd)
    if canon and acwd == canon:          # never reap a job pointed at canonical
        continue
    if selector == "__MISSING__":
        if os.path.isdir(cwd):
            continue
    elif acwd != os.path.abspath(selector):
        continue
    print(os.path.basename(os.path.dirname(sj)))
PY
}

# Best-effort retire the dead job records for a selector. Never fails the sweep
# (claude rm is now unblocked by the fixed WorktreeRemove hook); logs one line
# per reap. A missing `claude` binary is a silent no-op.
_reap_jobs() {
    local selector="$1" canonical="$2" job
    command -v claude >/dev/null 2>&1 || return 0
    while IFS= read -r job; do
        [[ -z "$job" ]] && continue
        if claude rm "$job" >/dev/null 2>&1; then
            echo "  reaped bg-job record $job (worktree archived)" >&2
        else
            echo "  reap: claude rm $job failed (non-fatal)" >&2
        fi
    done < <(_reap_job_candidates "$selector" "$canonical")
}

# All given PIDs reparented to pid 1 (orphans)? Unreadable ppid -> not-orphan
# (keep, never kill), preserving the under-reap bias.
_wt_all_orphans() {
    local pid ppid
    while IFS= read -r pid; do
        [[ -z "$pid" ]] && continue
        ppid="$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')"
        [[ -z "$ppid" || "$ppid" != "1" ]] && return 1
    done <<< "$1"
    return 0
}

_cargo_target_mtime() {
    stat -f %m "$1" 2>/dev/null || stat -c %Y "$1" 2>/dev/null || echo 0
}

_cargo_target_bytes() {
    local kib
    kib="$(du -sk "$1" 2>/dev/null | awk 'NR==1 {print $1}')"
    [[ "$kib" =~ ^[0-9]+$ ]] || kib=0
    echo $((kib * 1024))
}

_cargo_target_inventory() {
    local output="$1" wt target bytes mtime protection pids pids_rc
    : > "$output"
    _wt_refresh_cwd_snapshot || true
    while IFS= read -r wt; do
        [[ -d "$wt" ]] || continue
        protection="-"
        if _wt_live "$wt"; then
            protection="live-session"
        else
            pids="$(_wt_pids "$wt")"
            pids_rc=$?
            if [[ "$pids_rc" -ne 0 ]]; then
                protection="process-snapshot-unreadable"
            elif [[ -n "$pids" ]]; then
                protection="processes:$(printf '%s\n' "$pids" | grep -c .)"
            fi
        fi
        shopt -s nullglob
        for target in "$wt/target" "$wt"/crates/*/target; do
            [[ -d "$target" && ! -L "$target" ]] || continue
            bytes="$(_cargo_target_bytes "$target")"
            mtime="$(_cargo_target_mtime "$target")"
            printf '%s\t%s\t%s\t%s\t%s\n' "$mtime" "$bytes" "$protection" "$wt" "$target" >> "$output"
        done
        shopt -u nullglob
    done < <(git worktree list --porcelain 2>/dev/null | awk '/^worktree /{sub(/^worktree /, ""); print}')
}

_cargo_target_registered() {
    local wanted="$1"
    git worktree list --porcelain 2>/dev/null | awk '/^worktree /{sub(/^worktree /, ""); print}' | grep -Fqx "$wanted"
}

_cargo_target_path_is_owned() {
    local wt="$1" target="$2"
    [[ -d "$wt" && -d "$target" && ! -L "$target" ]] || return 1
    case "$target" in
        "$wt/target"|"$wt"/crates/*/target) return 0 ;;
        *) return 1 ;;
    esac
}

_cargo_target_cleanup() {
    local cap_bytes="$1" max_age_days="$2" apply="$3"
    local inventory candidates selected now before_bytes projected_after
    local mtime bytes protection wt target age_days reason pids
    local reaped=0 reclaimed=0 protected=0 after_bytes status mode

    if [[ ! "$cap_bytes" =~ ^[1-9][0-9]*$ ]]; then
        echo "cargo target cleanup: --cap-bytes must be a positive integer" >&2
        return 1
    fi
    if [[ ! "$max_age_days" =~ ^[0-9]+$ ]]; then
        echo "cargo target cleanup: --target-max-age must be Nd or a non-negative day count" >&2
        return 1
    fi

    inventory="$(mktemp -t fno-cargo-targets.XXXXXX)"
    candidates="$(mktemp -t fno-cargo-candidates.XXXXXX)"
    selected="$(mktemp -t fno-cargo-selected.XXXXXX)"
    : > "$candidates"
    : > "$selected"
    _cargo_target_inventory "$inventory"
    now="$(date +%s)"
    before_bytes="$(awk -F '\t' '{sum += $2} END {printf "%.0f", sum+0}' "$inventory")"
    projected_after="$before_bytes"

    while IFS=$'\t' read -r mtime bytes protection wt target; do
        [[ -n "$target" ]] || continue
        if [[ "$protection" != "-" ]]; then
            protected=$((protected + 1))
            printf 'cargo-target protected bytes=%s reason=%s path=%s\n' "$bytes" "$protection" "$target"
            continue
        fi
        printf '%s\t%s\t%s\t%s\n' "$mtime" "$bytes" "$wt" "$target" >> "$candidates"
    done < "$inventory"

    while IFS=$'\t' read -r mtime bytes wt target; do
        [[ -n "$target" ]] || continue
        age_days=$(( (now - mtime) / 86400 ))
        if [[ "$mtime" -le 0 || "$age_days" -lt "$max_age_days" ]]; then
            continue
        fi
        printf '%s\t%s\t%s\t%s\t%s\n' "$mtime" "$bytes" "$wt" "$target" "age" >> "$selected"
        projected_after=$((projected_after - bytes))
    done < <(sort -n "$candidates")

    if [[ "$projected_after" -gt "$cap_bytes" ]]; then
        while IFS=$'\t' read -r mtime bytes wt target; do
            [[ -n "$target" ]] || continue
            awk -F '\t' -v wanted="$target" '$4 == wanted { found=1 } END { exit !found }' "$selected" && continue
            printf '%s\t%s\t%s\t%s\t%s\n' "$mtime" "$bytes" "$wt" "$target" "cap" >> "$selected"
            projected_after=$((projected_after - bytes))
            [[ "$projected_after" -le "$cap_bytes" ]] && break
        done < <(sort -n "$candidates")
    fi

    mode="dry-run"
    if [[ -z "$apply" ]]; then
        while IFS=$'\t' read -r mtime bytes wt target reason; do
            [[ -n "$target" ]] || continue
            printf 'cargo-target would-reap bytes=%s reason=%s path=%s\n' "$bytes" "$reason" "$target"
        done < "$selected"
        status="ok"
        [[ "$projected_after" -gt "$cap_bytes" ]] && status="over-cap-protected"
        printf 'cargo-target-sweep status=%s mode=%s before_bytes=%s after_bytes=%s projected_after_bytes=%s cap_bytes=%s reaped=0 reclaimed_bytes=0 protected=%s\n' \
            "$status" "$mode" "$before_bytes" "$before_bytes" "$projected_after" "$cap_bytes" "$protected"
        unlink "$inventory" "$candidates" "$selected" 2>/dev/null || true
        [[ "$status" == "ok" ]]
        return $?
    fi

    mode="apply"
    _wt_refresh_cwd_snapshot || true
    while IFS=$'\t' read -r mtime bytes wt target reason; do
        [[ -n "$target" ]] || continue
        if ! _cargo_target_registered "$wt" || ! _cargo_target_path_is_owned "$wt" "$target"; then
            printf 'cargo-target kept bytes=%s reason=ownership-recheck path=%s\n' "$bytes" "$target"
            continue
        fi
        if _wt_live "$wt"; then
            printf 'cargo-target protected bytes=%s reason=live-session-recheck path=%s\n' "$bytes" "$target"
            protected=$((protected + 1))
            continue
        fi
        pids="$(_wt_pids "$wt")"
        pids_rc=$?
        if [[ "$pids_rc" -ne 0 ]]; then
            printf 'cargo-target protected bytes=%s reason=process-snapshot-unreadable path=%s\n' "$bytes" "$target"
            protected=$((protected + 1))
            continue
        fi
        if [[ -n "$pids" ]]; then
            printf 'cargo-target protected bytes=%s reason=process-recheck path=%s\n' "$bytes" "$target"
            protected=$((protected + 1))
            continue
        fi
        rm -rf -- "$target"
        if [[ ! -e "$target" ]]; then
            printf 'cargo-target reaped bytes=%s reason=%s path=%s\n' "$bytes" "$reason" "$target"
            reaped=$((reaped + 1))
            reclaimed=$((reclaimed + bytes))
        else
            printf 'cargo-target kept bytes=%s reason=delete-failed path=%s\n' "$bytes" "$target"
        fi
    done < "$selected"

    _cargo_target_inventory "$inventory"
    after_bytes="$(awk -F '\t' '{sum += $2} END {printf "%.0f", sum+0}' "$inventory")"
    status="ok"
    if [[ "$after_bytes" -gt "$cap_bytes" ]]; then
        status="over-cap-protected"
    fi
    printf 'cargo-target-sweep status=%s mode=%s before_bytes=%s after_bytes=%s projected_after_bytes=%s cap_bytes=%s reaped=%s reclaimed_bytes=%s protected=%s\n' \
        "$status" "$mode" "$before_bytes" "$after_bytes" "$after_bytes" "$cap_bytes" "$reaped" "$reclaimed" "$protected"
    unlink "$inventory" "$candidates" "$selected" 2>/dev/null || true
    [[ "$status" == "ok" ]]
}

case "${1:-status}" in
    status)
        shift
        # Cross-references the agents registry (real session names + measured
        # live/exited status) instead of `.fno/target-state.md`'s owner_pid,
        # which names the short-lived `fno do target init` CLI invocation and
        # reads as dead within seconds of session start - see
        # scripts/lib/worktree-status.py for the verified specimen.
        if command -v python3 >/dev/null 2>&1; then
            python3 "${_WT_LIFECYCLE_DIR}/worktree-status.py" --repo "$(pwd)" "$@"
        else
            echo "worktree status: python3 not found" >&2
            exit 1
        fi
        ;;

    cleanup)
        shift
        DAYS=7
        OLDER_SET=""
        DRY_RUN=""
        PREFIX=""
        MERGED=""
        APPLY=""
        KILL_ORPHANS=""
        CARGO_TARGETS=""
        CARGO_CAP_BYTES=68719476736
        CARGO_MAX_AGE_DAYS=7
        while [[ $# -gt 0 ]]; do
            case "$1" in
                --older-than) DAYS="${2%d}"; OLDER_SET="true"; shift 2 ;;
                --dry-run) DRY_RUN="true"; shift ;;
                --prefix) PREFIX="$2"; shift 2 ;;
                --merged) MERGED="true"; shift ;;
                --apply) APPLY="true"; shift ;;
                --kill-orphans) KILL_ORPHANS="true"; shift ;;
                --cargo-targets) CARGO_TARGETS="true"; shift ;;
                --cap-bytes) CARGO_CAP_BYTES="$2"; shift 2 ;;
                --target-max-age) CARGO_MAX_AGE_DAYS="${2%d}"; shift 2 ;;
                *) shift ;;
            esac
        done

        MAIN_DIR=$(git rev-parse --show-toplevel 2>/dev/null)

        # --- mutual exclusion --------------------------------------------------
        # A sweep is idempotent read-only-ish work (the --merged path only mutates
        # on --apply) that gains nothing from overlapping with another sweep - and
        # a concurrent sweep's own subprocesses carry every worktree path in their
        # argv, which _wt_pids' pgrep then matches, turning N overlapping sweeps
        # into an N-squared subprocess storm (measured: load 570, 159 chained
        # sweep processes, 2026-08-17). One sweep at a time removes that term
        # outright. Portable mkdir lock (atomic on every POSIX filesystem) so
        # there's no flock dependency; the status) case is never wrapped in this,
        # it stays a fast, always-answering read.
        _GIT_COMMON_DIR="$(git rev-parse --git-common-dir 2>/dev/null)"
        case "$_GIT_COMMON_DIR" in
            /*) ;;
            *) _GIT_COMMON_DIR="$MAIN_DIR/$_GIT_COMMON_DIR" ;;
        esac
        _WT_SWEEP_LOCK="$_GIT_COMMON_DIR/fno-wt-sweep.lock"
        _wt_lock_acquired=""
        for _wt_lock_attempt in 1 2 3 4 5; do
            if mkdir "$_WT_SWEEP_LOCK" 2>/dev/null; then
                _wt_lock_acquired=1
                break
            fi
            _held_pid="$(cat "$_WT_SWEEP_LOCK/pid" 2>/dev/null || true)"
            if [[ -n "$_held_pid" ]]; then
                if kill -0 "$_held_pid" 2>/dev/null; then
                    echo "worktree cleanup: another sweep (pid $_held_pid) is already running; exiting (sweeps are idempotent, no need to overlap)" >&2
                    exit 0
                fi
                # Stamped but dead: genuinely stale, reclaim it. rmdir only
                # succeeds on an empty dir, so a concurrent reclaimer that
                # wins the race makes ours fail here - loop and re-check
                # rather than mkdir blindly over a peer that just won.
                unlink "$_WT_SWEEP_LOCK/pid" 2>/dev/null || true
                rmdir "$_WT_SWEEP_LOCK" 2>/dev/null || true
                # Return to the atomic mkdir path. Recreating the directory
                # in this branch leaves a gap where two reclaimers can both
                # believe they acquired the lock before either writes its PID.
                continue
            fi
            # Dir exists but carries no pid yet: a peer may be mid-acquire
            # (mkdir succeeded, the pid write hasn't landed). Reclaiming this
            # unconditionally is the exact race that let two sweeps both
            # believe they held the lock - wait briefly instead of tearing
            # down a hold that never went stale.
            sleep 0.2
        done
        if [[ -z "$_wt_lock_acquired" ]]; then
            echo "worktree cleanup: could not acquire sweep lock after retries; exiting" >&2
            exit 0
        fi
        echo $$ > "$_WT_SWEEP_LOCK/pid"
        # Only tear down the lock if it still names us - a lock reclaimed
        # from a dead holder, or freshly acquired, must never be removed out
        # from under a different process that has since taken it over.
        trap '[[ "$(cat "$_WT_SWEEP_LOCK/pid" 2>/dev/null)" == "$$" ]] && { unlink "$_WT_SWEEP_LOCK/pid" 2>/dev/null || true; rmdir "$_WT_SWEEP_LOCK" 2>/dev/null || true; }' EXIT

        if [[ -n "$CARGO_TARGETS" ]]; then
            CARGO_APPLY="$APPLY"
            if [[ -n "$MERGED" || -n "$OLDER_SET" || -n "$PREFIX" || -n "$KILL_ORPHANS" ]]; then
                echo "worktree cleanup: --cargo-targets cannot be combined with worktree-removal selectors" >&2
                exit 1
            fi
            [[ -n "$DRY_RUN" ]] && CARGO_APPLY=""
            _cargo_target_cleanup "$CARGO_CAP_BYTES" "$CARGO_MAX_AGE_DAYS" "$CARGO_APPLY"
            exit $?
        fi

        # --- merged mode: reap worktrees whose branch already landed ---------
        if [[ -n "$MERGED" ]]; then
            if [[ -n "$OLDER_SET" ]]; then
                echo "worktree cleanup: --merged and --older-than are mutually exclusive" >&2
                exit 1
            fi
            ARCHIVE="$MAIN_DIR/scripts/setup/archive-worktree.sh"
            # True canonical checkout (first --porcelain entry), robust even when
            # the sweep runs from a worktree where --show-toplevel is the worktree.
            # Used only to guard job-record reaping off the canonical path.
            CANONICAL_MAIN="$(git worktree list --porcelain 2>/dev/null | awk 'NR==1{sub(/^worktree /,"");print}')"

            # One refresh up front, PRUNED: a branch deleted on the server
            # leaves its local tracking ref behind, and that stale ref would
            # vouch for a commit no remote carries (wt_unpushed_count) or a
            # phantom merged baseline. The shared predicate is
            # remote-agnostic, so a failure is sorted HERE, where the
            # origin-keyed baseline lives: origin unreachable aborts loudly
            # (silently keeping everything looks identical to a clean state,
            # so that failure must be loud); a dead NON-origin remote
            # degrades instead of bricking the sweep - detached trees are
            # kept (their refs cannot be verified) while origin-keyed
            # judging and the report continue.
            REFRESH_RC=0
            wt_refresh_remote_refs "$MAIN_DIR" || REFRESH_RC=$?
            if [[ "$REFRESH_RC" -ne 0 ]] && ! git fetch --prune origin >/dev/null 2>&1; then
                echo "worktree cleanup --merged: refresh of origin failed; aborting (refs would be stale)" >&2
                exit 1
            fi
            if ! git rev-parse --verify --quiet origin/main >/dev/null 2>&1; then
                echo "worktree cleanup --merged: origin/main does not resolve after fetch; aborting" >&2
                exit 1
            fi

            N_TOTAL=0; N_REAP=0; N_FAIL=0
            N_DIRTY=0; N_UNPUSHED=0; N_UNMERGED=0; N_LIVE=0; N_PROC=0; N_SALVAGE=0; N_NEEDCONF=0; N_APP_OWNED=0; N_PERM=0

            _wt_refresh_cwd_snapshot || true
            printf '%-18s %-34s %s\n' "STATUS" "BRANCH" "PATH"
            while IFS= read -r wt; do
                [[ "$wt" == "$MAIN_DIR" ]] && continue

                branch="$(git -C "$wt" rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)"
                head="$(git -C "$wt" rev-parse HEAD 2>/dev/null || echo '')"

                # Honor --prefix scoping in merged mode too: a scoped sweep must
                # never touch (or count) a branch outside its prefix.
                if [[ -n "$PREFIX" && "$branch" != ${PREFIX}* ]]; then
                    continue
                fi
                N_TOTAL=$((N_TOTAL + 1))

                # Codex Desktop owns snapshot/removal for its managed
                # worktrees. Keep them even after merge; archiving the chat is
                # the supported cleanup primitive.
                if _wt_app_owned "$wt"; then
                    printf '%-18s %-34s %s\n' "kept (app-owned)" "$branch" "$wt"; N_APP_OWNED=$((N_APP_OWNED + 1)); continue
                fi

                # 0a. permanent by design (_wt_permanent).
                if _wt_permanent "$wt"; then
                    printf '%-18s %-34s %s\n' "kept (permanent)" "$branch" "$wt"; N_PERM=$((N_PERM + 1)); continue
                fi

                # 1. holds content removal would destroy (tracked only; no
                #    --ignored so the .fno symlink family is not "dirty"). A
                #    tracked file MISSING from disk is recoverable from HEAD, so
                #    it never blocks - see scripts/lib/worktree-reapable.sh. The
                #    receipt names the recoverable-deletion count so a
                #    systematic cause is visible instead of the word "dirty".
                if ! wt_reapable "$wt"; then
                    # Print the reason, or the promise above is empty: "dirty"
                    # alone cannot tell an untracked scratch file from a probe
                    # that never answered.
                    reason="${WT_REAPABLE_LINE#*reason=}"; reason="${reason%% *}"
                    printf '%-18s %-34s %s  (%s)\n' "kept (dirty)" "$branch" "$wt" "$reason"; N_DIRTY=$((N_DIRTY + 1)); continue
                fi
                # 2. merged into origin/main? A detached HEAD is judged by
                #    content, not by the branch-name proxy: the tree is kept
                #    only while it holds commits no remote carries
                #    (wt_unpushed_count fails toward keep), because scratch
                #    trees are detached BY CONSTRUCTION and a blanket keep
                #    meant the disk-reclaim verb could never reap the
                #    population that grows. Branched trees keep the
                #    merged-or-upstream logic below unchanged.
                if [[ "$branch" == "HEAD" || -z "$head" ]]; then
                    if [[ "$(wt_unpushed_count "$wt")" -gt 0 ]]; then
                        # The fail-safe count (1) is indistinguishable from a
                        # real one in the status column, so the row names an
                        # unverifiable refresh instead of asserting unpushed
                        # commits that may not exist.
                        if [[ "${_WT_REMOTE_REFS_FRESH:-0}" == 1 ]]; then
                            printf '%-18s %-34s %s\n' "kept (unpushed)" "$branch" "$wt"
                        else
                            printf '%-18s %-34s %s\n' "kept (unpushed)" "$branch" "$wt  (remote refs unverifiable)"
                        fi
                        N_UNPUSHED=$((N_UNPUSHED + 1)); continue
                    fi
                elif ! git -C "$wt" merge-base --is-ancestor "$head" origin/main 2>/dev/null; then
                    # Not in main. Local-only commits (data loss) = unpushed;
                    # pushed to its own remote but not in main = unmerged (safe).
                    up="$(git -C "$wt" rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null || true)"
                    if [[ -n "$up" ]]; then
                        ahead="$(git -C "$wt" rev-list --count "$up"..HEAD 2>/dev/null || echo 1)"
                        if [[ "$ahead" -gt 0 ]]; then
                            printf '%-18s %-34s %s\n' "kept (unpushed)" "$branch" "$wt"; N_UNPUSHED=$((N_UNPUSHED + 1)); continue
                        fi
                        printf '%-18s %-34s %s\n' "kept (unmerged)" "$branch" "$wt"; N_UNMERGED=$((N_UNMERGED + 1)); continue
                    fi
                    printf '%-18s %-34s %s\n' "kept (unpushed)" "$branch" "$wt"; N_UNPUSHED=$((N_UNPUSHED + 1)); continue
                fi
                # 3. live session
                if _wt_live "$wt"; then
                    printf '%-18s %-34s %s\n' "kept (live-session)" "$branch" "$wt"; N_LIVE=$((N_LIVE + 1)); continue
                fi
                # 4. rooted processes
                YES=""
                pids="$(_wt_pids "$wt")"
                pids_rc=$?
                if [[ "$pids_rc" -ne 0 ]]; then
                    printf '%-18s %-34s %s\n' "kept (process-snapshot-unreadable)" "$branch" "$wt"; N_PROC=$((N_PROC + 1)); continue
                fi
                if [[ -n "$pids" ]]; then
                    if [[ -z "$KILL_ORPHANS" ]]; then
                        printf '%-18s %-34s %s\n' "kept (processes: $(printf '%s\n' "$pids" | grep -c .))" "$branch" "$wt"; N_PROC=$((N_PROC + 1)); continue
                    fi
                    if _wt_all_orphans "$pids"; then
                        YES="--yes"   # archive-worktree.sh SIGTERMs the ppid-1 orphans
                    else
                        printf '%-18s %-34s %s\n' "kept (live-session)" "$branch" "$wt"; N_LIVE=$((N_LIVE + 1)); continue
                    fi
                fi
                # Candidate. Dry-run is the default for --merged, and an
                # explicit --dry-run wins even if --apply was also passed
                # (a safety wrapper appending --dry-run must never be ignored).
                if [[ -z "$APPLY" || -n "$DRY_RUN" ]]; then
                    printf '%-18s %-34s %s\n' "would-archive" "$branch" "$wt"; N_REAP=$((N_REAP + 1)); continue
                fi
                if [[ ! -f "$ARCHIVE" ]]; then
                    printf '%-18s %-34s %s\n' "failed (no-script)" "$branch" "$wt"; N_FAIL=$((N_FAIL + 1)); continue
                fi
                # Salvage + strict re-check + removal all live in archive-worktree.sh
                # (its liveness re-check at removal time is authoritative, not our
                # cached one). Exit 5 = salvage kept the worktree.
                bash "$ARCHIVE" "$wt" $YES >&2
                rc=$?
                case "$rc" in
                    0) printf '%-18s %-34s %s\n' "archived" "$branch" "$wt"; N_REAP=$((N_REAP + 1))
                       _reap_jobs "$wt" "$CANONICAL_MAIN" ;;
                    3) printf '%-18s %-34s %s\n' "kept (needs-confirmation)" "$branch" "$wt"; N_NEEDCONF=$((N_NEEDCONF + 1)) ;;
                    5) printf '%-18s %-34s %s\n' "kept (salvage-failed)" "$branch" "$wt"; N_SALVAGE=$((N_SALVAGE + 1)) ;;
                    6) printf '%-18s %-34s %s\n' "kept (app-owned)" "$branch" "$wt"; N_APP_OWNED=$((N_APP_OWNED + 1)) ;;
                    *) printf '%-18s %-34s %s\n' "failed (rc=$rc)" "$branch" "$wt"; N_FAIL=$((N_FAIL + 1)) ;;
                esac
            done < <(git worktree list --porcelain 2>/dev/null | awk '/^worktree /{sub(/^worktree /, ""); print}')

            # Final pass (apply only): retire dead job records whose worktree
            # path is already gone - e.g. a pr-merged-<n> worktree archived by
            # an EARLIER sweep, leaving the job row dangling in the agents view.
            if [[ -n "$APPLY" && -z "$DRY_RUN" ]]; then
                _reap_jobs "__MISSING__" "$CANONICAL_MAIN"
            fi

            KEPT=$((N_DIRTY + N_UNPUSHED + N_UNMERGED + N_LIVE + N_PROC + N_SALVAGE + N_NEEDCONF + N_APP_OWNED + N_PERM))
            echo ""
            if [[ "$N_TOTAL" -eq 0 ]]; then
                echo "No non-canonical worktrees found."
            else
                EXECUTED=""; [[ -n "$APPLY" && -z "$DRY_RUN" ]] && EXECUTED="1"
                VERB="would archive"; [[ -n "$EXECUTED" ]] && VERB="archived"
                SUFFIX=""; [[ -z "$EXECUTED" ]] && SUFFIX="  [dry-run: no changes made; pass --apply to execute]"
                printf 'Summary: %d %s, %d kept (%d unmerged, %d unpushed, %d dirty, %d live-session, %d processes, %d salvage-failed, %d needs-confirmation, %d app-owned, %d permanent), %d failed%s\n' \
                    "$N_REAP" "$VERB" "$KEPT" "$N_UNMERGED" "$N_UNPUSHED" "$N_DIRTY" "$N_LIVE" "$N_PROC" "$N_SALVAGE" "$N_NEEDCONF" "$N_APP_OWNED" "$N_PERM" "$N_FAIL" "$SUFFIX"
            fi
            exit 0
        fi

        REMOVED=0

        _wt_refresh_cwd_snapshot || true
        while IFS= read -r wt; do
            # Skip main repo
            [[ "$wt" == "$MAIN_DIR" ]] && continue

            # Filter by prefix if specified
            if [[ -n "$PREFIX" ]]; then
                BRANCH=$(cd "$wt" 2>/dev/null && git branch --show-current || echo "")
                [[ "$BRANCH" != ${PREFIX}* ]] && continue
            fi

            if _wt_app_owned "$wt"; then
                echo "  SKIP: $wt (app-owned Codex worktree)"
                continue
            fi

            # Permanent by design on BOTH removal paths (_wt_permanent): the
            # preflight tree resets to a fresh candidate per run (so its
            # commit age reads zero while active) and goes 7+ days stale the
            # moment preflight stops running, which is exactly when this age
            # sweep fires.
            if _wt_permanent "$wt"; then
                echo "  SKIP: $wt (permanent preflight worktree)"
                continue
            fi

            # Check age
            LAST_COMMIT=$(cd "$wt" 2>/dev/null && git log -1 --format="%ct" 2>/dev/null || echo 0)
            NOW=$(date +%s)
            AGE_DAYS=$(( (NOW - LAST_COMMIT) / 86400 ))

            if [[ $AGE_DAYS -ge $DAYS ]]; then
                # Check target
                STATUS=$(grep '^status:' "$wt/.fno/target-state.md" 2>/dev/null | awk '{print $2}')
                if [[ "$STATUS" == "IN_PROGRESS" ]]; then
                    echo "  SKIP: $wt (active target session)"
                    continue
                fi

                BRANCH=$(cd "$wt" 2>/dev/null && git branch --show-current || echo "unknown")
                # A detached tree has no branch to preserve, so the --force
                # below would destroy any commit no remote carries - the exact
                # loss the merged sweep's wt_unpushed_count guard prevents.
                # The refresh must run in THIS shell: the count below runs in
                # a $( ) subshell that cannot carry the freshness flag back,
                # so refreshing only inside it re-fetches per detached tree.
                if [[ -z "$BRANCH" ]]; then
                    # Uncommitted content first, before any network: a detached
                    # tree has no branch holding it, so --force destroys an
                    # untracked or modified file as surely as an unpushed
                    # commit. Same classifier as the merged sweep; the
                    # in-flight marker cli/src/fno/evals/runner.py drops is
                    # untracked and rides this guard.
                    if ! wt_reapable "$wt"; then
                        reason="${WT_REAPABLE_LINE#*reason=}"; reason="${reason%% *}"
                        echo "  SKIP: $wt (detached HEAD holds uncommitted work: $reason)"
                        continue
                    fi
                    wt_refresh_remote_refs "$wt" >/dev/null 2>&1 || true
                    if [[ "$(wt_unpushed_count "$wt")" -gt 0 ]]; then
                        if [[ "${_WT_REMOTE_REFS_FRESH:-0}" == 1 ]]; then
                            echo "  SKIP: $wt (detached HEAD holds unpushed commits)"
                        else
                            echo "  SKIP: $wt (remote refs unverifiable; detached HEAD may hold unpushed commits)"
                        fi
                        continue
                    fi
                fi
                pids="$(_wt_pids "$wt")"
                pids_rc=$?
                if [[ "$pids_rc" -ne 0 ]]; then
                    echo "  SKIP: $wt (process snapshot unreadable)"
                    continue
                fi
                if [[ -n "$pids" ]]; then
                    echo "  SKIP: $wt (processes: $(printf '%s\n' "$pids" | grep -c .))"
                    continue
                fi
                if [[ -n "$DRY_RUN" ]]; then
                    echo "  WOULD REMOVE: $wt ($AGE_DAYS days old, branch: $BRANCH)"
                else
                    if git worktree remove --force "$wt" 2>/dev/null; then
                        echo "  REMOVED: $wt (branch $BRANCH preserved)"
                        REMOVED=$((REMOVED + 1))
                    else
                        echo "  FAILED: $wt could not be removed (try: git worktree prune)"
                    fi
                fi
            fi
        done < <(git worktree list --porcelain 2>/dev/null | grep "^worktree " | sed 's/^worktree //')

        if [[ -z "$DRY_RUN" ]]; then
            echo "Cleanup complete. Removed $REMOVED worktree(s)."
        fi
        ;;

    archive)
        NAME="${2:-}"
        if [[ -z "$NAME" ]]; then
            echo "Usage: worktree-lifecycle.sh archive <worktree-name>"
            exit 1
        fi

        WT=".claude/worktrees/$NAME"
        if [[ -d "$WT" ]]; then
            BRANCH=$(cd "$WT" && git branch --show-current)
            if git worktree remove --force "$WT" 2>/dev/null; then
                echo "Archived: directory removed, branch $BRANCH preserved in git"
            else
                echo "Archive FAILED: $WT could not be removed (try: git worktree prune)"
                exit 1
            fi
        else
            echo "Worktree not found: $WT"
            exit 1
        fi
        ;;

    *)
        echo "Usage: worktree-lifecycle.sh {status|cleanup|archive} [args]"
        exit 1
        ;;
esac
