#!/usr/bin/env bash
# Worktree lifecycle management
# Usage:
#   worktree-lifecycle.sh status                    # List all worktrees
#   worktree-lifecycle.sh cleanup [--older-than Nd] [--prefix <prefix>] [--apply] [--dry-run]
#   worktree-lifecycle.sh cleanup --merged [--apply]
#   Both cleanup removal modes are dry-run by default; --apply executes.
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

if [[ -f "${_WT_LIFECYCLE_DIR}/worktree-removal-event.sh" ]]; then
    # shellcheck source=/dev/null
    source "${_WT_LIFECYCLE_DIR}/worktree-removal-event.sh"
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

# The per-hit occupancy classifier over _wt_pids output (x-0396). A partial
# deploy that dropped the lib degrades to all-holds: every tree with a
# process is kept, never killed.
if [[ -f "${_WT_LIFECYCLE_DIR}/worktree-occupancy.sh" ]]; then
    # shellcheck source=/dev/null
    source "${_WT_LIFECYCLE_DIR}/worktree-occupancy.sh"
else
    wt_classify_pids() { _wt_occupancy_failclosed "$2"; }
    _wt_occupancy_failclosed() {
        local pid
        while IFS= read -r pid; do
            [[ -z "$pid" ]] && continue
            printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$pid" holds keep - "classifier unavailable" -
        done <<< "$1"
    }
    wt_occupancy_print_rows() { printf '%s\n' "$1" | awk -F '\t' 'NF >= 6 { printf "    %s %s %s | %s\n", $1, $2, $5, $6 }'; }
fi

# Process start identity for the sweep lock stamp. A partial deploy without
# the lib prints nothing, which degrades to pid-only stamps and today's
# kill -0 behavior.
if [[ -f "${_WT_LIFECYCLE_DIR}/events-lock.sh" ]]; then
    # shellcheck source=/dev/null
    source "${_WT_LIFECYCLE_DIR}/events-lock.sh"
else
    _event_process_identity() { :; }
fi

# --- merged-mode helpers (used only by `cleanup --merged`) ------------------

# Live target session? The manifest's `status:` field (legacy era) was once
# read here as liveness, but the field is WRITE-ONCE: a session that died with
# `status: IN_PROGRESS` carries it forever, so the grep kept every crashed
# legacy tree eternally live. Liveness truth is the node claim (session-pid
# anchored + TTL) plus the process lane in the sweep; a legacy manifest with no
# claim key has neither, and an IN_PROGRESS string is not a third signal.
# owner_pid is checked last and only as a positive signal: it is the transient
# `fno do target init` wrapper pid, dead about a second after init returns, so
# on its own this returned 1 for every live session and the merged-cleanup
# sweep would prune a running target's worktree.
_wt_live() {
    local st="$1/.fno/target-state.md"
    [[ -f "$st" ]] || return 1
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
# Snapshot-time evidence for the candidates _wt_pids kept, pid:cmd@cwd,
# comma-joined. A later `ps -p` re-read lies: the process can die in between.
_WT_PIDS_DIAG=""
# Row counts the last _wt_pids decision read from each snapshot, plus the
# diagnostic-format version the protected line stamps. A verdict that
# contradicts these counters means an older script ran.
_WT_PIDS_DIAG_VERSION=6

_wt_refresh_cwd_snapshot() {
    local raw=""
    _WT_CWD_SNAPSHOT=""
    _WT_CWD_SNAPSHOT_OK=0
    command -v lsof >/dev/null 2>&1 || return 1
    # lsof runs from / (subshell cd, parent unaffected) so its own pipeline
    # never lands in the snapshot with the sweep's cwd: run it from the repo
    # root and the tree the sweep runs in - the canonical checkout - reads
    # processes:2 with nobody in it, the lsof and its formatting fork. The
    # awk below runs after raw is captured, so it cannot appear in raw.
    raw="$(cd / && lsof -a -d cwd -Fpn 2>/dev/null)" || return 1
    _WT_CWD_SNAPSHOT="$(printf '%s\n' "$raw" | awk '
        /^p[0-9]+$/ { pid = substr($0, 2); next }
        /^n/ && pid != "" { print pid "\t" substr($0, 2) }
    ')"
    _WT_CWD_ROWS="$(printf '%s\n' "${_WT_CWD_SNAPSHOT:-}" | awk 'NF { n++ } END { print n + 0 }')"
    _WT_CWD_SNAPSHOT_OK=1
}

_wt_pids() {
    local wt="$1" root pids="" pids_f="" re candidates filtered snapshot_rc=0
    local ps_rc=0 ps_out="" ps_snap="" ps_rows=0
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
        _WT_PIDS_DIAG=""
        return "$snapshot_rc"
    fi
    # One process-table snapshot for the whole candidate set, not one `ps`
    # subprocess per pid: a concurrent sweep's own argv carries every
    # worktree path (see the lock comment above), so candidates scale with
    # the number of overlapping sweeps and a per-pid `ps` turned that into
    # N sweeps x 49 worktrees x N matches. ppid rides the SAME snapshot: the
    # ancestor drop below reads it in memory, never via a second ps call.
    # The marker keeps awk's first input non-empty and positively identifies a
    # completed snapshot; otherwise an empty ps makes the candidates FNR==NR.
    ps_rc=0
    ps_out="$(ps -Ao pid=,ppid=,command= 2>/dev/null)" || ps_rc=$?
    ps_snap="$(printf '%s\n' "$ps_out"; printf '%s\n' '__FNO_PS_SNAPSHOT_COMPLETE__')"
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
            sub("^" pid "[ \t]+[^ \t]+[ \t]+", "", line)
            cmdbypid[pid] = line
            ppidbypid[pid] = $2
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
    ' <(printf '%s\n' "$ps_snap") <(printf '%s\n' "$candidates"))"
    # The drops below fire only on a positively-populated ps snapshot: an
    # empty one (fork-starved sweep, sandbox denies ps) proves nothing, so
    # every candidate is kept fail-closed and the diagnostic records the ps
    # exit status plus any cwd sighting, readable from the protection line.
    ps_rows="$(awk -v m="__FNO_PS_SNAPSHOT_COMPLETE__" '$0 == m { exit } NF { c++ } END { print c + 0 }' <<< "$ps_snap")"
    local filtered2="" pid_keep cwd_row kept_info kp kcmd kcwd
    _WT_PIDS_DIAG=""
    if [[ "$ps_rows" -eq 0 ]]; then
        filtered2="$filtered"
        while IFS= read -r pid_keep; do
            [[ -z "$pid_keep" ]] && continue
            cwd_row="$(printf '%s\n' "${_WT_CWD_SNAPSHOT:-}" \
                | awk -F '\t' -v want="$pid_keep" '$1 == want { print $2; exit }')"
            _WT_PIDS_DIAG="${_WT_PIDS_DIAG}${pid_keep}:no-ps@${cwd_row:-no-cwd-row},"
        done <<< "$filtered"
        _WT_PIDS_DIAG="ps-rc=${ps_rc} rows=0 ${_WT_PIDS_DIAG%,}"
        printf '%s\n' "$_WT_PIDS_DIAG" >&2
        printf '%s\n' "$filtered2"
        return "$snapshot_rc"
    fi
    # Survivors resolved in ONE awk pass over the in-memory snapshots, never
    # one fork per candidate: under a fork-starved sweep the per-candidate
    # awk calls multiply the very pressure that emptied the ps snapshot
    # (CI smoke 2026-09-07, ps-rc diagnostic). Three drops, all reading the
    # same snapshot:
    # 1. machinery (done in the first awk) and ancestors of this sweep: the
    #    candidate's ppid chain is walked to $$ in memory;
    # 2. zombies and enumeration transients: a ps row whose command column
    #    is empty or defunct owns no fds, no cwd, no mmap, and cannot hold
    #    build artifacts;
    # 3. a candidate with a live ps row, or a cwd-snapshot sighting, stays.
    filtered2=""
    # bash 3.2 cannot parse nested quotes inside ${var:-"..."}: the empty
    # cwd snapshot rides as a placeholder row instead.
    cwd_input="${_WT_CWD_SNAPSHOT}"
    if [[ -z "$cwd_input" ]]; then
        cwd_input="-"
    fi
    kept_info="$(awk -v self="$$" '
        FNR == 1 { stage++ }
        stage == 1 {
            line = $0
            if (line == "__FNO_PS_SNAPSHOT_COMPLETE__") {
                seen_complete = 1
                next
            }
            sub(/^[ \t]+/, "", line)
            pid = $1
            sub("^" pid "[ \t]+[^ \t]+[ \t]+", "", line)
            cmdbypid[pid] = line
            ppidbypid[pid] = $2
            next
        }
        stage == 2 {
            if (!walked && seen_complete) {
                # The ANCESTORS of the sweep: walk UP from $$ once, after
                # the ppid map is complete. A candidate IN this set is the
                # sweep invoker. Walking from the CANDIDATE upward and
                # hitting $$ would read the opposite: a descendant of the
                # sweep, and a descendant anchored in the tree is a real
                # occupant (the battery pins this).
                p = self
                for (i = 0; i < 12 && p != "" && p != "0" && p != "1"; i++) {
                    mine[p] = 1
                    p = ppidbypid[p]
                }
                walked = 1
                selfcmd = cmdbypid[self]
            }
            if ($0 == "-") next
            cwdbypid[$1] = $2
            next
        }
        stage == 3 {
            pid = $1
            if (pid in mine) next
            # A child of the sweep carrying the same command line as the
            # sweep itself is the command-substitution subshell running this
            # very check (CI smoke 2026-09-07, pid 6327: bash <script-path>):
            # it lives for the whole function, so its ps row is live, and it
            # forked after the cwd snapshot, so no cwd row can clear it. A
            # real occupant has a different command line or another parent.
            if (ppidbypid[pid] == self && cmdbypid[pid] == selfcmd) next
            cmd = cmdbypid[pid]
            if (cmd != "" && cmd !~ /defunct/) {
                print pid "\t" cmd "\t-"
                next
            }
            if (pid in cwdbypid) {
                print pid "\tno-ps-row\t" cwdbypid[pid]
            }
        }
    ' <(printf '%s\n' "$ps_snap") <(printf '%s\n' "$cwd_input") <(printf '%s\n' "$filtered"))"
    while IFS=$'\t' read -r kp kcmd kcwd; do
        [[ -z "$kp" ]] && continue
        filtered2="${filtered2}${kp}"$'\n'
        _WT_PIDS_DIAG="${_WT_PIDS_DIAG}${kp}:${kcmd:0:60}@${kcwd:0:60},"
    done <<< "$kept_info"
    # The row count rides inside the diagnostic: _wt_pids normally runs in a
    # command substitution, and globals it sets die with that subshell. The
    # stderr copy is what the parent actually reads.
    _WT_PIDS_DIAG="ps-rows=${ps_rows} ${_WT_PIDS_DIAG%,}"
    printf '%s\n' "$_WT_PIDS_DIAG" >&2
    printf '%s\n' "$filtered2"
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
# per reap. A missing `claude` binary is a silent no-op. Optional extra args
# are job ids named by the occupancy classifier's retire rows (x-0396): the
# sweep retires the job whose session process it released, not just ones the
# cwd-keyed candidate query can see (that field is the spawn directory).
_reap_jobs() {
    local selector="$1" canonical="$2" job
    shift 2
    command -v claude >/dev/null 2>&1 || return 0
    {
        _reap_job_candidates "$selector" "$canonical"
        if [[ $# -gt 0 ]]; then printf '%s\n' "$@"; fi
    } | sort -u | while IFS= read -r job; do
        [[ -z "$job" ]] && continue
        if claude rm "$job" >/dev/null 2>&1; then
            echo "  reaped bg-job record $job (worktree archived)" >&2
        else
            # retired-ok: reports which shellout failed on which job.
            echo "  reap: claude rm $job failed (non-fatal)" >&2
        fi
    done
}

_cargo_target_mtime() {
    local value=""
    value="$(stat -f %m "$1" 2>/dev/null)" || value=""
    if [[ "$value" =~ ^[0-9]+$ ]]; then
        printf '%s\n' "$value"
        return 0
    fi
    value="$(stat -c %Y "$1" 2>/dev/null)" || value=""
    [[ "$value" =~ ^[0-9]+$ ]] || value=0
    printf '%s\n' "$value"
}

_cargo_target_bytes() {
    local kib
    kib="$(du -sk "$1" 2>/dev/null | awk 'NR==1 {print $1}')"
    [[ "$kib" =~ ^[0-9]+$ ]] || kib=0
    echo $((kib * 1024))
}

_cargo_target_inventory() {
    local output="$1" wt target bytes mtime protection prot pids pids_rc resolved
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
            prot="$protection"
            if [[ -L "$target" ]]; then
                # A cache the retired offload verb relocated. The sweep follows the link
                # or the relocation strands the bytes with no reclaimer at
                # all. Following is gated by BOTH conjuncts of
                # _cargo_cache_dir_owned (under an fno base, tagged
                # CACHEDIR.TAG); bytes and age read from the RESOLVED dir
                # (BSD du does not follow a command-line symlink). A link
                # failing either conjunct rides the protected lane as
                # link-not-owned: counted, reported, never deleted. prot is
                # per-target - the worktree-level protection must not leak
                # into sibling target rows.
                resolved="$(cd -- "$target" 2>/dev/null && pwd -P)" || continue
                bytes="$(_cargo_target_bytes "$resolved")"
                mtime="$(_cargo_target_mtime "$resolved")"
                if [[ "$prot" == "-" ]] \
                    && ! _cargo_cache_dir_owned "$resolved"; then
                    prot="link-not-owned"
                fi
            elif [[ -d "$target" ]]; then
                bytes="$(_cargo_target_bytes "$target")"
                mtime="$(_cargo_target_mtime "$target")"
            else
                continue
            fi
            printf '%s\t%s\t%s\t%s\t%s\n' "$mtime" "$bytes" "$prot" "$wt" "$target" >> "$output"
        done
        shopt -u nullglob
    done < <(git worktree list --porcelain 2>/dev/null | awk '/^worktree /{sub(/^worktree /, ""); print}')
    # Build-base hash dirs: cargo writes intermediates at
    # <base>/<h2>/<hash> under build.build-dir, outside every checkout, so
    # the worktree walk above never sees them. Rows carry wt=build-base;
    # _cargo_target_cleanup protects the dirs live workspaces resolve to and
    # never deletes here when that resolution is unverifiable.
    local base hash
    base="$(_cargo_build_base)"
    if [[ -d "$base" ]]; then
        shopt -s nullglob
        for hash in "$base"/*/*/; do
            [[ -d "$hash" ]] || continue
            hash="${hash%/}"
            [[ -f "$hash/CACHEDIR.TAG" ]] || continue
            bytes="$(_cargo_target_bytes "$hash")"
            mtime="$(_cargo_target_mtime "$hash")"
            printf '%s\t%s\t%s\t%s\t%s\n' "$mtime" "$bytes" "-" "build-base" "$hash" >> "$output"
        done
        shopt -u nullglob
    fi
}

_cargo_target_registered() {
    local wanted="$1"
    git worktree list --porcelain 2>/dev/null | awk '/^worktree /{sub(/^worktree /, ""); print}' | grep -Fqx "$wanted"
}

_cargo_live_build_dirs() {
    # Resolved build_directory (cargo metadata, one call per workspace) of
    # every live registered worktree, one path per line. cargo >= 1.91
    # reports the field the tracked config's build-dir lands in. Exit 1 when
    # any read fails: the caller must then treat EVERY build-base dir as
    # protected, because a blind sweep is the one mistake this lane cannot
    # undo.
    local wt manifest
    while IFS= read -r wt; do
        _wt_live "$wt" || continue
        for manifest in "$wt"/crates/*/Cargo.toml; do
            [[ -f "$manifest" ]] || continue
            cargo metadata --format-version 1 --no-deps --manifest-path "$manifest" 2>/dev/null \
                | grep -o '"build_directory"[[:space:]]*:[[:space:]]*"[^"]*"' \
                | sed 's/.*:[[:space:]]*"//; s/"$//' || return 1
        done
    done < <(git worktree list --porcelain 2>/dev/null | awk '/^worktree /{sub(/^worktree /, ""); print}')
}

_cargo_target_path_is_owned() {
    local wt="$1" target="$2" resolved=""
    if [[ "$wt" == "build-base" ]]; then
        # A hash-dir row: owned iff it still sits under a managed base and
        # carries cargo's tag. Registration is the base itself.
        [[ -d "$target" ]] || return 1
        _cargo_cache_dir_owned "$target" || return 1
        return 0
    fi
    case "$target" in
        "$wt/target"|"$wt"/crates/*/target) ;;
        *) return 1 ;;
    esac
    if [[ -L "$target" ]]; then
        # Relocated cache: the link must sit where a glob found it AND the
        # resolved directory must be one the offload created.
        resolved="$(cd -- "$target" 2>/dev/null && pwd -P)" || return 1
        _cargo_cache_dir_owned "$resolved" || return 1
        return 0
    fi
    [[ -d "$wt" && -d "$target" ]] || return 1
    return 0
}

_cargo_free_bytes() {
    # Available bytes on the volume holding $1. FNO_CARGO_FREE_BYTES overrides
    # the read so tests can simulate a low-free disk without filling one.
    if [[ -n "${FNO_CARGO_FREE_BYTES:-}" ]]; then
        printf '%s\n' "$FNO_CARGO_FREE_BYTES"
        return 0
    fi
    df -Pk "$1" 2>/dev/null | awk 'NR==2 {print $4*1024}'
}

_cargo_build_base() {
    # Where cargo intermediates live: paths.cargo_targets_base when set, else
    # ~/.fno/cargo-build. FNO_CARGO_TARGETS_BASE overrides the read so tests
    # can point the sweep at a sandbox. Mirrors fno.paths.cargo_build_dir_value.
    local raw=""
    if [[ -n "${FNO_CARGO_TARGETS_BASE:-}" ]]; then
        printf '%s\n' "${FNO_CARGO_TARGETS_BASE/#\~/$HOME}"
        return 0
    fi
    if command -v fno >/dev/null 2>&1; then
        raw="$(fno config get config.paths.cargo_targets_base 2>/dev/null || true)"
    fi
    # The state-dir fallback form is the shape the hardcoded-path gate
    # exempts: honor a configured state_dir, else the standard ~/.fno.
    [[ "$raw" == "null" || -z "$raw" ]] && raw="${STATE_DIR:-$HOME/.fno}/cargo-build"
    # Config stores ~ literally; expand a leading ~ to $HOME.
    printf '%s\n' "${raw/#\~/$HOME}"
}

_cargo_legacy_offload_base() {
    # The retired offload verb's relocation base. Its symlinks survive in
    # old trees; the sweep removes each link together with its resolved dir
    # when the dir sits under THIS base and is tagged. Never configurable: the
    # base died with the verb, and a config key would teach it back.
    printf '%s\n' "${STATE_DIR:-$HOME/.fno}/cargo-targets"
}

_cargo_cache_dir_owned() {
    # Is $1 a directory this repo's tooling created: under the build base (or
    # the retired offload base) AND carrying cargo's own CACHEDIR.TAG? Both
    # conjuncts are load-bearing: the base is fno-owned, the tag is cargo's,
    # so a symlink reaching outside either is not ours to delete. Both sides
    # are normalised through pwd -P: /tmp is a symlink to /private/tmp, and a
    # logical row path never matches a physical base prefix.
    local resolved="$1" base
    [[ -d "$resolved" ]] || return 1
    [[ -f "$resolved/CACHEDIR.TAG" ]] || return 1
    resolved="$(cd -- "$resolved" 2>/dev/null && pwd -P)" || return 1
    for base in "$(_cargo_build_base)" "$(_cargo_legacy_offload_base)"; do
        base="$(cd -- "$base" 2>/dev/null && pwd -P)" || continue
        case "$resolved/" in
            "$base/"*) return 0 ;;
        esac
    done
    return 1
}

_cargo_target_cleanup() {
    local cap_bytes="$1" max_age_days="$2" apply="$3" free_share_pct="${4:-50}"
    local inventory candidates selected now before_bytes projected_after
    local mtime bytes protection wt target age_days reason pids resolved
    local reaped=0 reclaimed=0 protected=0 after_bytes status mode
    local free_bytes effective_cap_bytes

    if [[ ! "$cap_bytes" =~ ^[1-9][0-9]*$ ]]; then
        echo "cargo target cleanup: --cap-bytes must be a positive integer" >&2
        return 1
    fi
    if [[ ! "$max_age_days" =~ ^[0-9]+$ ]]; then
        echo "cargo target cleanup: --target-max-age must be Nd or a non-negative day count" >&2
        return 1
    fi
    if [[ ! "$free_share_pct" =~ ^[0-9]+$ ]] || [[ "$free_share_pct" -lt 1 || "$free_share_pct" -gt 100 ]]; then
        echo "cargo target cleanup: --free-share-pct must be an integer between 1 and 100" >&2
        return 1
    fi

    # The absolute cap alone is a floor the sweep defends on a nearly full
    # disk (measured live: 63 GiB allocated, 4.2 GB free, "ok", 0 reaped).
    # The effective ceiling is min(absolute cap, free-share percent of free
    # space) so a full disk tightens it; an unreadable free space falls
    # back to the absolute cap and is reported as free_bytes=unknown. The
    # digit bound keeps free*pct inside 64-bit arithmetic.
    free_bytes="$(_cargo_free_bytes "${MAIN_DIR:-$(pwd)}")"
    if [[ "$free_bytes" =~ ^[1-9][0-9]{0,14}$ ]]; then
        effective_cap_bytes=$(( free_bytes * free_share_pct / 100 ))
        [[ "$effective_cap_bytes" -gt "$cap_bytes" ]] && effective_cap_bytes="$cap_bytes"
        [[ "$effective_cap_bytes" -lt 1 ]] && effective_cap_bytes=1
    else
        free_bytes="unknown"
        effective_cap_bytes="$cap_bytes"
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

    # Build-base protection: the hash dirs LIVE workspaces resolve to. An
    # unreadable resolution (no cargo, a bad manifest) marks every build-base
    # row unverifiable - protected this run, never deleted blind.
    local live_build_dirs="" build_dirs_unverifiable=0
    if ! live_build_dirs="$(_cargo_live_build_dirs)"; then
        build_dirs_unverifiable=1
        live_build_dirs=""
    fi

    while IFS=$'\t' read -r mtime bytes protection wt target; do
        [[ -n "$target" ]] || continue
        if [[ "$protection" != "-" ]]; then
            protected=$((protected + 1))
            printf 'cargo-target protected bytes=%s reason=%s path=%s\n' "$bytes" "$protection" "$target"
            continue
        fi
        if [[ "$wt" == "build-base" ]]; then
            if [[ "$build_dirs_unverifiable" == "1" ]]; then
                protected=$((protected + 1))
                printf 'cargo-target protected bytes=%s reason=build-dir-unverifiable path=%s\n' "$bytes" "$target"
                continue
            fi
            if printf '%s\n' "$live_build_dirs" | grep -Fqx "$target"; then
                protected=$((protected + 1))
                printf 'cargo-target protected bytes=%s reason=live-workspace-build-dir path=%s\n' "$bytes" "$target"
                continue
            fi
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

    if [[ "$projected_after" -gt "$effective_cap_bytes" ]]; then
        while IFS=$'\t' read -r mtime bytes wt target; do
            [[ -n "$target" ]] || continue
            awk -F '\t' -v wanted="$target" '$4 == wanted { found=1 } END { exit !found }' "$selected" && continue
            printf '%s\t%s\t%s\t%s\t%s\n' "$mtime" "$bytes" "$wt" "$target" "cap" >> "$selected"
            projected_after=$((projected_after - bytes))
            [[ "$projected_after" -le "$effective_cap_bytes" ]] && break
        done < <(sort -n "$candidates")
    fi

    mode="dry-run"
    if [[ -z "$apply" ]]; then
        while IFS=$'\t' read -r mtime bytes wt target reason; do
            [[ -n "$target" ]] || continue
            printf 'cargo-target would-reap bytes=%s reason=%s path=%s\n' "$bytes" "$reason" "$target"
        done < "$selected"
        status="ok"
        [[ "$projected_after" -gt "$effective_cap_bytes" ]] && status="over-cap-protected"
        printf 'cargo-target-sweep status=%s mode=%s before_bytes=%s after_bytes=%s projected_after_bytes=%s cap_bytes=%s free_bytes=%s effective_cap_bytes=%s reaped=0 reclaimed_bytes=0 protected=%s\n' \
            "$status" "$mode" "$before_bytes" "$before_bytes" "$projected_after" "$cap_bytes" "$free_bytes" "$effective_cap_bytes" "$protected"
        unlink "$inventory" "$candidates" "$selected" 2>/dev/null || true
        [[ "$status" == "ok" ]]
        return $?
    fi

    mode="apply"
    _wt_refresh_cwd_snapshot || true
    # Re-resolve live workspaces' build dirs for the delete pass: selection
    # and deletion are separate walks over the same inventory, and a session
    # that went live in between must find its hash dir protected here too.
    local apply_live_dirs="" apply_unverifiable=0
    if ! apply_live_dirs="$(_cargo_live_build_dirs)"; then
        apply_unverifiable=1
        apply_live_dirs=""
    fi
    while IFS=$'\t' read -r mtime bytes wt target reason; do
        [[ -n "$target" ]] || continue
        if ! _cargo_target_path_is_owned "$wt" "$target"; then
            printf 'cargo-target kept bytes=%s reason=ownership-recheck path=%s\n' "$bytes" "$target"
            continue
        fi
        if [[ "$wt" == "build-base" ]]; then
            # Registration recheck does not apply (the base is the registrar)
            # and there is no cwd to be rooted in; the live guard is the
            # resolved-dir membership above, re-read for this pass.
            if [[ "$apply_unverifiable" == "1" ]] \
                || printf '%s\n' "$apply_live_dirs" | grep -Fqx "$target"; then
                printf 'cargo-target protected bytes=%s reason=live-workspace-build-dir path=%s\n' "$bytes" "$target"
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
            continue
        fi
        if ! _cargo_target_registered "$wt"; then
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
        if [[ -L "$target" ]]; then
            # Relocated cache: delete the RESOLVED directory first, then the
            # link - rm -rf on a symlink alone never touches the bytes. The
            # ownership recheck above already validated it, but resolve and
            # re-verify here anyway: the check and the delete are separate
            # walks, and the cheap conjuncts are what keep rm inside the base.
            resolved="$(cd -- "$target" 2>/dev/null && pwd -P)" || resolved=""
            if [[ -z "$resolved" ]] || ! _cargo_cache_dir_owned "$resolved"; then
                printf 'cargo-target kept bytes=%s reason=link-target-not-owned path=%s\n' "$bytes" "$target"
                continue
            fi
            rm -rf -- "$resolved"
            rm -f -- "$target"
            if [[ ! -e "$resolved" && ! -L "$target" ]]; then
                printf 'cargo-target reaped bytes=%s reason=%s path=%s\n' "$bytes" "$reason" "$target"
                reaped=$((reaped + 1))
                reclaimed=$((reclaimed + bytes))
            else
                printf 'cargo-target kept bytes=%s reason=delete-failed path=%s\n' "$bytes" "$target"
            fi
        else
            rm -rf -- "$target"
            if [[ ! -e "$target" ]]; then
                printf 'cargo-target reaped bytes=%s reason=%s path=%s\n' "$bytes" "$reason" "$target"
                reaped=$((reaped + 1))
                reclaimed=$((reclaimed + bytes))
            else
                printf 'cargo-target kept bytes=%s reason=delete-failed path=%s\n' "$bytes" "$target"
            fi
        fi
    done < "$selected"

    _cargo_target_inventory "$inventory"
    after_bytes="$(awk -F '\t' '{sum += $2} END {printf "%.0f", sum+0}' "$inventory")"
    status="ok"
    if [[ "$after_bytes" -gt "$effective_cap_bytes" ]]; then
        status="over-cap-protected"
    fi
    printf 'cargo-target-sweep status=%s mode=%s before_bytes=%s after_bytes=%s projected_after_bytes=%s cap_bytes=%s free_bytes=%s effective_cap_bytes=%s reaped=%s reclaimed_bytes=%s protected=%s\n' \
        "$status" "$mode" "$before_bytes" "$after_bytes" "$after_bytes" "$cap_bytes" "$free_bytes" "$effective_cap_bytes" "$reaped" "$reclaimed" "$protected"
    unlink "$inventory" "$candidates" "$selected" 2>/dev/null || true
    [[ "$status" == "ok" ]]
}

# One sweep at a time, shared by cleanup callers. The lock lives in the
# git common dir, resolved absolutely so the answer holds from any cwd;
# the function leaves the trap armed on success.
_acquire_sweep_lock() {
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
    _GIT_COMMON_DIR="$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
    _lock_why=""
    if [[ -z "$_GIT_COMMON_DIR" ]]; then
        _lock_why="git rev-parse --git-common-dir answered nothing from cwd $PWD"
    elif [[ ! -d "$_GIT_COMMON_DIR" ]]; then
        _lock_why="$_GIT_COMMON_DIR is not a directory"
    elif [[ ! -w "$_GIT_COMMON_DIR" ]]; then
        _lock_why="$_GIT_COMMON_DIR is not writable"
    fi
    if [[ -n "$_lock_why" ]]; then
        echo "worktree cleanup: no usable sweep lock directory: $_lock_why. This is not lock contention; retrying will not help." >&2
        exit 1
    fi
    _WT_SWEEP_LOCK="$_GIT_COMMON_DIR/fno-wt-sweep.lock"
    # The sweep's own birth certificate, for the budget-expiry grace
    # below: a directory OLDER than this file predates the sweep and can
    # be nobody's live claim.
    _WT_SWEEP_STARTED="$_GIT_COMMON_DIR/.fno-wt-sweep-started.$$"
    : > "$_WT_SWEEP_STARTED" 2>/dev/null || _WT_SWEEP_STARTED=""
    # The full teardown trap below is armed only on acquire; until then this
    # lighter trap keeps the exits that never reach the lock (another holder,
    # exhausted retries) from leaking the certificate into the common dir.
    trap 'rm -f "$_WT_SWEEP_STARTED" 2>/dev/null || true' EXIT
    # Our own stamp: pid plus start identity, judged by _wt_holder_live. A
    # pid-only stamp (identity unavailable) keeps the legacy kill -0 meaning.
    _WT_SELF_STAMP="$$"
    _wt_self_started="$(_wt_stamp_identity $$)"
    [[ -n "$_wt_self_started" ]] && _WT_SELF_STAMP="$$"$'\n'"$_wt_self_started"
    _wt_lock_acquired=""
    for _wt_lock_attempt in 1 2 3 4 5; do
        if mkdir "$_WT_SWEEP_LOCK" 2>/dev/null; then
            # The claim is not HELD until our own pid is the one on disk:
            # the steal window below can take a fresh mkdir away before
            # the write lands, and a sweep that proceeded on a lost
            # directory wedged every later sweep behind a pid-less lock.
            printf '%s\n' "$_WT_SELF_STAMP" > "$_WT_SWEEP_LOCK/pid" 2>/dev/null || true
            if [[ "$(cat "$_WT_SWEEP_LOCK/pid" 2>/dev/null || true)" == "$_WT_SELF_STAMP" ]]; then
                _wt_lock_acquired=1
                break
            fi
            continue
        fi
        _held_stamp="$(cat "$_WT_SWEEP_LOCK/pid" 2>/dev/null || true)"
        if [[ -n "$_held_stamp" ]]; then
            _held_pid="${_held_stamp%%$'\n'*}"
            _holder_rc=0
            _wt_holder_live "$_held_stamp" || _holder_rc=$?
            if [[ "$_held_pid" == "$$" ]]; then
                # Our OWN lost claim: the verify above rejected it, so
                # this directory is ours to reclaim - backing off to
                # ourselves would read as "another sweep is running"
                # and strand our pid on the path until it dies. A stamp
                # naming our pid but stamped by a predecessor the pid
                # number moved to is ours by the same right.
                unlink "$_WT_SWEEP_LOCK/pid" 2>/dev/null || true
                rmdir "$_WT_SWEEP_LOCK" 2>/dev/null || true
                continue
            fi
            if [[ "$_holder_rc" -eq 0 ]]; then
                echo "worktree cleanup: another sweep (pid $_held_pid) is already running; exiting (sweeps are idempotent, no need to overlap)" >&2
                # A legacy stamp has no start line, so this refusal may name
                # a recycled pid; the operator can clear the path by hand.
                if [[ "$_held_stamp" != *$'\n'* ]]; then
                    echo "worktree cleanup: that lock records no start time. If pid $_held_pid is not a worktree sweep, remove $_WT_SWEEP_LOCK and retry." >&2
                fi
                exit 0
            fi
            if [[ "$_holder_rc" -eq 2 ]]; then
                echo "worktree cleanup: sweep lock pid $_held_pid is alive but started $(_wt_stamp_identity "$_held_pid"), not ${_held_stamp#*$'\n'}; the pid was reused, reclaiming the lock" >&2
            fi
            # Stamped but dead (or a recycled pid the stamp exposes):
            # reclaim it. The steal must take the directory that was
            # OBSERVED, and the observed directory's identity is its pid
            # file, byte for byte. A blind removal acts on an observation
            # that is already stale when it lands: the stale dir may have
            # been replaced by a peer's fresh claim in between, and eating
            # that is the ABA shape that ended with two sweeps both holding
            # the lock. An inode match is NOT identity either: on Linux the
            # directory created right after one is deleted can reuse the
            # freed inode, and CI proved it - the moved FRESH claim matched
            # and was eaten. A successor carries no pid file (fresh claim)
            # or its own live stamp, never the observed dead one; a live
            # stamp on the moved copy keeps the steal off, and a start-time
            # mismatch (a recycled pid) reads as dead through the same
            # helper the alive branch used, so the two answers cannot
            # diverge. The steal target is per-attempt unique and
            # pre-cleaned, so mv always renames rather than nesting into a
            # leftover of a killed earlier steal.
            _WT_STALE="$_WT_SWEEP_LOCK.stale.$$.$RANDOM"
            rm -rf "$_WT_STALE" 2>/dev/null || true
            mv "$_WT_SWEEP_LOCK" "$_WT_STALE" 2>/dev/null || true
            if [[ -d "$_WT_STALE" ]]; then
                _moved_stamp="$(cat "$_WT_STALE/pid" 2>/dev/null || true)"
                if [[ -n "$_moved_stamp" && "$_moved_stamp" == "$_held_stamp" ]] \
                    && ! _wt_holder_live "$_moved_stamp"; then
                    rm -rf "$_WT_STALE"
                elif [[ ! -e "$_WT_SWEEP_LOCK" ]]; then
                    # Not what we observed and nobody has claimed the path
                    # since: put it back untouched.
                    # ACCEPTED RESIDUAL: a verified holder can still be
                    # dislodged here by a double-steal chain - our stale
                    # observation outlives a peer's steal-and-verify, our
                    # mv takes the peer's verified dir, and a third claim
                    # mkdirs inside the test-to-mv window so the restore
                    # NESTS the peer's copy and the lift deletes it. The
                    # lift stays (without it the holder's own trap wedges
                    # on a non-empty dir); the window is milliseconds wide
                    # and no shell primitive closes it in place - the
                    # atomic-claim substrate is the real retirement, filed
                    # separately.
                    mv "$_WT_STALE" "$_WT_SWEEP_LOCK"
                    # A claim taking the path between the test and this mv
                    # makes the restore NEST (mv moves a dir into an
                    # existing dir); lifting our copy back out leaves the
                    # holder's own trap able to rmdir later.
                    if [[ -d "$_WT_SWEEP_LOCK/${_WT_STALE##*/}" ]]; then
                        rm -rf "$_WT_SWEEP_LOCK/${_WT_STALE##*/}"
                    fi
                elif [[ -z "$_moved_stamp" ]] || ! _wt_holder_live "$_moved_stamp"; then
                    # The path was re-taken before the restore, so the
                    # moved copy is unreachable debris; reap it only when
                    # its own stamp is absent or dead - never while it
                    # names a live claim.
                    rm -rf "$_WT_STALE"
                fi
                # A live-stamp copy with no free path stays where it is;
                # the sibling sweep at the next acquisition reaps it once
                # that process dies.
            fi
            # Return to the atomic mkdir path.
            continue
        fi
        # Dir exists but carries no pid yet: a peer may be mid-acquire
        # (mkdir succeeded, the pid write hasn't landed). Reclaiming this
        # unconditionally is the exact race that let two sweeps both
        # believe they held the lock - wait briefly instead of tearing
        # down a hold that never went stale.
        if [[ "$_wt_lock_attempt" -eq 5 ]]; then
            # Still pid-less after the whole retry budget: not a
            # mid-acquire peer (its write lands in milliseconds) but
            # debris from a lost pid write. Reaping an EMPTY dir here is
            # what keeps one lost write from wedging every future sweep,
            # and the empty-only rmdir is also the guard: a peer's pid
            # write landing between the emptiness read and here makes the
            # dir non-empty and the rmdir fails, so a holder is never
            # removed. The deeper wedge is a PID-LESS dir WITH content -
            # an interrupted steal's nested copy, whose owner's trap
            # unlinked the pid but could not rmdir - which the rmdir gives
            # up on silently and every future sweep expires against. That
            # one gets rm -rf, only once it is OLDER than this sweep
            # (find -newer against the birth certificate above): anything
            # created after the sweep started is someone's live claim and
            # is spared.
            if [[ -z "$(ls -A "$_WT_SWEEP_LOCK" 2>/dev/null || true)" ]]; then
                rmdir "$_WT_SWEEP_LOCK" 2>/dev/null || true
            elif [[ -n "$_WT_SWEEP_STARTED" ]] \
                && [[ -n "$(find "$_WT_SWEEP_LOCK" -maxdepth 0 ! -newer "$_WT_SWEEP_STARTED" 2>/dev/null)" ]]; then
                rm -rf "$_WT_SWEEP_LOCK"
            fi
        fi
        sleep 0.2
    done
    if [[ -z "$_wt_lock_acquired" ]]; then
        echo "worktree cleanup: could not acquire sweep lock after retries at $_WT_SWEEP_LOCK; exiting" >&2
        exit 0
    fi
    # Sweep-leftover debris: a steal interrupted between the mv and its
    # disposition leaves fno-wt-sweep.lock.stale.* siblings nothing ever
    # revisits. Holding the lock makes every sibling unreferenced; each
    # is reaped only while its own stamp is absent or dead, so a live
    # claim's copy survives until that process dies.
    for _wt_stale in "$_GIT_COMMON_DIR"/fno-wt-sweep.lock.stale.*; do
        [[ -d "$_wt_stale" ]] || continue
        _stale_stamp="$(cat "$_wt_stale/pid" 2>/dev/null || true)"
        if [[ -z "$_stale_stamp" ]] || ! _wt_holder_live "$_stale_stamp"; then
            rm -rf "$_wt_stale"
        fi
    done
    # Birth certificates from sweeps that died between creating the file and
    # arming a trap: the certificate is read only inside a live sweep's retry
    # loop (about a second), so one older than five minutes has no reader.
    find "$_GIT_COMMON_DIR" -maxdepth 1 -name '.fno-wt-sweep-started.*' -mmin +5 -exec rm -f {} + 2>/dev/null || true
    printf '%s\n' "$_WT_SELF_STAMP" > "$_WT_SWEEP_LOCK/pid" 2>/dev/null || true
    # Only tear down the lock if it still names us - a lock reclaimed
    # from a dead holder, or freshly acquired, must never be removed out
    # from under a different process that has since taken it over.
    trap 'rm -f "$_WT_SWEEP_STARTED" 2>/dev/null || true; [[ "$(cat "$_WT_SWEEP_LOCK/pid" 2>/dev/null)" == "$_WT_SELF_STAMP" ]] && { unlink "$_WT_SWEEP_LOCK/pid" 2>/dev/null || true; rmdir "$_WT_SWEEP_LOCK" 2>/dev/null || true; }' EXIT
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
        CARGO_TARGETS=""
        CARGO_CAP_BYTES=68719476736
        CARGO_MAX_AGE_DAYS=7
        CARGO_FREE_SHARE_PCT=50
        while [[ $# -gt 0 ]]; do
            case "$1" in
                --older-than) DAYS="${2%d}"; OLDER_SET="true"; shift 2 ;;
                --dry-run) DRY_RUN="true"; shift ;;
                --prefix) PREFIX="$2"; shift 2 ;;
                --merged) MERGED="true"; shift ;;
                --apply) APPLY="true"; shift ;;
                --kill-orphans)
                    # Retired (x-0396): the ppid-1 leg released trees by
                    # parentage, which a real pane keeper reads as safe to
                    # kill. Classified processes release a tree now; every
                    # unclassified one always keeps it.
                    echo "worktree cleanup: --kill-orphans is retired: classified processes release a tree by default, unclassified ones always keep it" >&2
                    shift ;;
                --cargo-targets) CARGO_TARGETS="true"; shift ;;
                --cap-bytes) CARGO_CAP_BYTES="$2"; shift 2 ;;
                --free-share-pct) CARGO_FREE_SHARE_PCT="$2"; shift 2 ;;
                --target-max-age) CARGO_MAX_AGE_DAYS="${2%d}"; shift 2 ;;
                *) shift ;;
            esac
        done

        MAIN_DIR=$(git rev-parse --show-toplevel 2>/dev/null)

        _acquire_sweep_lock

        if [[ -n "$CARGO_TARGETS" ]]; then
            CARGO_APPLY="$APPLY"
            if [[ -n "$MERGED" || -n "$OLDER_SET" || -n "$PREFIX" ]]; then
                echo "worktree cleanup: --cargo-targets cannot be combined with worktree-removal selectors" >&2
                exit 1
            fi
            [[ -n "$DRY_RUN" ]] && CARGO_APPLY=""
            _cargo_target_cleanup "$CARGO_CAP_BYTES" "$CARGO_MAX_AGE_DAYS" "$CARGO_APPLY" "$CARGO_FREE_SHARE_PCT"
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
                elif ! wt_head_reachable_from_origin_main "$wt"; then
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
                # 4. rooted processes, each named and classified (x-0396). A
                #    hit the classifier cannot positively place is a holder;
                #    absence of a recognised holder is never proof the tree
                #    is free.
                YES=""
                ROWS=""
                RETIRE_JOBS=""
                pids="$(_wt_pids "$wt")"
                pids_rc=$?
                if [[ "$pids_rc" -ne 0 ]]; then
                    printf '%-18s %-34s %s\n' "kept (process-snapshot-unreadable)" "$branch" "$wt"; N_PROC=$((N_PROC + 1)); continue
                fi
                if [[ -n "$pids" ]]; then
                    ROWS="$(wt_classify_pids "$wt" "$pids")"
                    N_HELD="$(printf '%s\n' "$ROWS" | awk -F '\t' '$2 == "holds" { c++ } END { print c + 0 }')"
                    N_INERT="$(printf '%s\n' "$ROWS" | awk -F '\t' '$2 == "inert" { c++ } END { print c + 0 }')"
                    if [[ "$N_HELD" -gt 0 ]]; then
                        printf '%-18s %-34s %s\n' "kept (processes: $N_HELD held, $N_INERT inert)" "$branch" "$wt"
                        wt_occupancy_print_rows "$ROWS"
                        N_PROC=$((N_PROC + 1)); continue
                    fi
                    # All inert: the tree may go. Retire rows carry the claude
                    # job ids to release alongside the archive (best effort).
                    RETIRE_JOBS="$(printf '%s\n' "$ROWS" | awk -F '\t' '$4 != "-" { print $4 }' | sort -u | tr '\n' ' ')"
                    RETIRE_JOBS="${RETIRE_JOBS% }"
                fi
                # Candidate. Dry-run is the default for --merged, and an
                # explicit --dry-run wins even if --apply was also passed
                # (a safety wrapper appending --dry-run must never be ignored).
                if [[ -z "$APPLY" || -n "$DRY_RUN" ]]; then
                    printf '%-18s %-34s %s\n' "would-archive" "$branch" "$wt"
                    [[ -n "$ROWS" ]] && wt_occupancy_print_rows "$ROWS"
                    N_REAP=$((N_REAP + 1)); continue
                fi
                if [[ ! -f "$ARCHIVE" ]]; then
                    printf '%-18s %-34s %s\n' "failed (no-script)" "$branch" "$wt"; N_FAIL=$((N_FAIL + 1)); continue
                fi
                # Salvage + strict re-check + removal all live in archive-worktree.sh
                # (its liveness re-check at removal time is authoritative, not our
                # cached one). Exit 5 = salvage kept the worktree. The caller env
                # names this path in the worktree_removed event row it emits. No
                # --yes is passed: the removal-time classification re-reads the
                # tree and only inert rows are signalled (x-0396).
                FNO_WT_REMOVE_CALLER="cleanup --merged" bash "$ARCHIVE" "$wt" $YES >&2
                rc=$?
                case "$rc" in
                    0) printf '%-18s %-34s %s\n' "archived" "$branch" "$wt"; N_REAP=$((N_REAP + 1))
                       # shellcheck disable=SC2086
                       _reap_jobs "$wt" "$CANONICAL_MAIN" $RETIRE_JOBS ;;
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
        WOULD=0

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
                # Live-session check via _wt_live (claim-anchored). The legacy
                # `status: IN_PROGRESS` grep that stood here kept crashed trees
                # alive forever (write-once field); the claim lane is the real
                # signal, and it now guards BOTH removal paths.
                if _wt_live "$wt"; then
                    echo "  SKIP: $wt (active target session)"
                    continue
                fi

                BRANCH=$(cd "$wt" 2>/dev/null && git branch --show-current || echo "unknown")
                # Uncommitted content first, before any network, for EVERY tree.
                # The guard used to fire only on detached HEADs, but a branched
                # tree with uncommitted work hits the same `--force` remove and
                # loses it just as surely - the branch never recorded it. Same
                # classifier as the merged sweep; the in-flight marker
                # cli/src/fno/evals/runner.py drops is untracked and rides this
                # guard. DIRTY is never touched by any automatic path.
                if ! wt_reapable "$wt"; then
                    reason="${WT_REAPABLE_LINE#*reason=}"; reason="${reason%% *}"
                    echo "  SKIP: $wt (holds uncommitted work: $reason)"
                    continue
                fi
                # A detached tree has no branch to preserve, so the --force
                # below would destroy any commit no remote carries - the exact
                # loss the merged sweep's wt_unpushed_count guard prevents.
                # The refresh must run in THIS shell: the count below runs in
                # a $( ) subshell that cannot carry the freshness flag back,
                # so refreshing only inside it re-fetches per detached tree.
                if [[ -z "$BRANCH" ]]; then
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
                # Dry-run is the default for BOTH removal modes; an explicit
                # --dry-run wins even if --apply was also passed (a safety
                # wrapper appending --dry-run must never be ignored), mirroring
                # the --merged mode's precedence.
                if [[ -z "$APPLY" || -n "$DRY_RUN" ]]; then
                    echo "  WOULD REMOVE: $wt ($AGE_DAYS days old, branch: $BRANCH)"
                    WOULD=$((WOULD + 1))
                else
                    if git worktree remove --force "$wt" 2>/dev/null; then
                        echo "  REMOVED: $wt (branch $BRANCH preserved)"
                        REMOVED=$((REMOVED + 1))
                        if declare -F _wt_emit_removal_event >/dev/null 2>&1; then
                            _wt_emit_removal_event "$MAIN_DIR" "$wt" "cleanup --older-than" \
                                "no-live-claim (guard passed)" \
                                "age>=${DAYS}d + reapable passed" "$BRANCH" false
                        fi
                    else
                        echo "  FAILED: $wt could not be removed (try: git worktree prune)"
                    fi
                fi
            fi
        done < <(git worktree list --porcelain 2>/dev/null | grep "^worktree " | sed 's/^worktree //')

        if [[ -z "$APPLY" || -n "$DRY_RUN" ]]; then
            echo "Cleanup complete (dry-run). Would remove $WOULD worktree(s). Pass --apply to execute."
        else
            echo "Cleanup complete. Removed $REMOVED worktree(s)."
        fi
        ;;

    archive)
        shift
        NAME=""
        ARCHIVE_ARGS=()
        while [[ $# -gt 0 ]]; do
            case "$1" in
                --force|--yes|-y|--delete-branch)
                    ARCHIVE_ARGS+=("$1")
                    shift
                    ;;
                --)
                    shift
                    if [[ $# -ne 1 ]]; then
                        echo "Usage: worktree-lifecycle.sh archive [--force] [--yes] [--delete-branch] <name|path>" >&2
                        exit 1
                    fi
                    NAME="$1"
                    shift
                    ;;
                -*)
                    echo "worktree archive: unknown flag: $1" >&2
                    exit 1
                    ;;
                *)
                    if [[ -n "$NAME" ]]; then
                        echo "worktree archive: expected one worktree name or path" >&2
                        exit 1
                    fi
                    NAME="$1"
                    shift
                    ;;
            esac
        done
        if [[ -z "$NAME" ]]; then
            echo "Usage: worktree-lifecycle.sh archive <worktree-name>"
            exit 1
        fi

        ARCHIVE_SCRIPT="${_WT_LIFECYCLE_DIR}/../setup/archive-worktree.sh"
        if [[ ! -f "$ARCHIVE_SCRIPT" ]]; then
            echo "worktree archive: guarded script not found at $ARCHIVE_SCRIPT" >&2
            exit 1
        fi
        if [[ ${#ARCHIVE_ARGS[@]} -gt 0 ]]; then
            exec bash "$ARCHIVE_SCRIPT" "${ARCHIVE_ARGS[@]}" "$NAME"
        else
            exec bash "$ARCHIVE_SCRIPT" "$NAME"
        fi
        ;;


    *)
        echo "Usage: worktree-lifecycle.sh {status|cleanup|archive} [args]"
        exit 1
        ;;
esac
