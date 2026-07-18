#!/usr/bin/env bash
# Worktree lifecycle management
# Usage:
#   worktree-lifecycle.sh status                    # List all worktrees
#   worktree-lifecycle.sh cleanup [--older-than Nd] [--dry-run] [--prefix <prefix>]
#   worktree-lifecycle.sh cleanup --merged [--apply] [--kill-orphans]
#   worktree-lifecycle.sh archive <name>            # Keep branch, remove directory
set -uo pipefail

# --- merged-mode helpers (used only by `cleanup --merged`) ------------------

# Live target session? Legacy manifests carried status: IN_PROGRESS; the modern
# immutable manifest has no status field, so a live owner_pid is the signal.
_wt_live() {
    local st="$1/.fno/target-state.md"
    [[ -f "$st" ]] || return 1
    grep -qE '^status:[[:space:]]*IN_PROGRESS' "$st" && return 0
    local pid
    # Pipeline-free extraction so a no-match never SIGPIPEs an upstream grep.
    pid="$(sed -nE '/^owner_pid:[[:space:]]*[0-9]+/{s/^owner_pid:[[:space:]]*//;p;q;}' "$st" 2>/dev/null)"
    [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null && return 0
    return 1
}

# PIDs actually rooted in the worktree (cwd under it) OR whose cmdline
# references it. Mirrors archive-worktree.sh's enumeration (escaped regex so
# path metachars are literal); drops our own PID and our own tooling.
#
# `lsof -a -d cwd +D` (NOT bare `+D`) keys on the cwd fd so uv-hardlinked venv
# `.so` files mmapped by long-lived daemons - the same inode under every
# worktree - never count as "rooted here". The pgrep lane still catches bg
# processes carrying the path in argv.
_wt_pids() {
    local wt="$1" pids="" pids_f="" re
    if command -v lsof >/dev/null 2>&1; then
        pids="$(lsof -a -d cwd +D "$wt" 2>/dev/null | awk 'NR>1 {print $2}' | sort -u)"
    fi
    re="$(printf '%s' "$wt" | sed -e 's/[][\\.^$*+?(){}|/]/\\&/g')"
    pids_f="$(pgrep -f -- "$re" 2>/dev/null || true)"
    # Drop our own PID and any live pid running the sweep/archive tooling: a
    # concurrent sweep carries the worktree path in its argv (a different PGID,
    # so pgrep -f matches it), and it is our own machinery, never a squatter.
    # `|| true`: a mid-pipeline `grep -v` with no match exits 1, which pipefail
    # would surface as the function's status even though the pids printed fine.
    printf '%s\n%s\n' "$pids" "$pids_f" | grep -v "^$$\$" | grep -v '^$' | sort -u \
        | while IFS= read -r pid; do
            case "$(ps -o command= -p "$pid" 2>/dev/null)" in
                *archive-worktree.sh*|*worktree-lifecycle.sh*) continue ;;
            esac
            printf '%s\n' "$pid"
        done || true
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

case "${1:-status}" in
    status)
        echo "Worktrees:"
        git worktree list --porcelain 2>/dev/null | while IFS= read -r line; do
            case "$line" in
                "worktree "*)
                    WT_PATH="${line#worktree }"
                    ;;
                "branch "*)
                    BRANCH="${line#branch refs/heads/}"
                    # Check target status
                    TARGET=""
                    if [[ -f "$WT_PATH/.fno/target-state.md" ]]; then
                        TARGET=$(grep '^status:' "$WT_PATH/.fno/target-state.md" 2>/dev/null | awk '{print $2}')
                    elif [[ -L "$WT_PATH/.fno" && -f "$WT_PATH/.fno/target-state.md" ]]; then
                        TARGET=$(grep '^status:' "$WT_PATH/.fno/target-state.md" 2>/dev/null | awk '{print $2}')
                    fi
                    # Last commit age
                    LAST=$(cd "$WT_PATH" 2>/dev/null && git log -1 --format="%cr" 2>/dev/null || echo "unknown")
                    printf "  %-30s | %-15s | target: %-12s | %s\n" "$BRANCH" "$LAST" "${TARGET:-none}" "$WT_PATH"
                    ;;
            esac
        done
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
        while [[ $# -gt 0 ]]; do
            case "$1" in
                --older-than) DAYS="${2%d}"; OLDER_SET="true"; shift 2 ;;
                --dry-run) DRY_RUN="true"; shift ;;
                --prefix) PREFIX="$2"; shift 2 ;;
                --merged) MERGED="true"; shift ;;
                --apply) APPLY="true"; shift ;;
                --kill-orphans) KILL_ORPHANS="true"; shift ;;
                *) shift ;;
            esac
        done

        MAIN_DIR=$(git rev-parse --show-toplevel 2>/dev/null)

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

            # One fetch up front. A failure aborts loudly rather than reaping
            # against stale refs (silently keeping everything looks identical
            # to a clean state, so the failure must be loud).
            if ! git fetch origin main >/dev/null 2>&1; then
                echo "worktree cleanup --merged: git fetch origin main failed; aborting (refs would be stale)" >&2
                exit 1
            fi
            if ! git rev-parse --verify --quiet origin/main >/dev/null 2>&1; then
                echo "worktree cleanup --merged: origin/main does not resolve after fetch; aborting" >&2
                exit 1
            fi

            N_TOTAL=0; N_REAP=0; N_FAIL=0
            N_DIRTY=0; N_UNPUSHED=0; N_UNMERGED=0; N_LIVE=0; N_PROC=0; N_SALVAGE=0; N_NEEDCONF=0

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

                # 1. dirty (tracked only; no --ignored so the .fno symlink family is not "dirty")
                if [[ -n "$(git -C "$wt" status --porcelain 2>/dev/null)" ]]; then
                    printf '%-18s %-34s %s\n' "kept (dirty)" "$branch" "$wt"; N_DIRTY=$((N_DIRTY + 1)); continue
                fi
                # 2. merged into origin/main? Detached HEAD (deleted branch) is always kept.
                if [[ "$branch" == "HEAD" || -z "$head" ]]; then
                    printf '%-18s %-34s %s\n' "kept (unmerged)" "$branch" "$wt"; N_UNMERGED=$((N_UNMERGED + 1)); continue
                fi
                if ! git -C "$wt" merge-base --is-ancestor "$head" origin/main 2>/dev/null; then
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
                    *) printf '%-18s %-34s %s\n' "failed (rc=$rc)" "$branch" "$wt"; N_FAIL=$((N_FAIL + 1)) ;;
                esac
            done < <(git worktree list --porcelain 2>/dev/null | awk '/^worktree /{sub(/^worktree /, ""); print}')

            # Final pass (apply only): retire dead job records whose worktree
            # path is already gone - e.g. a pr-merged-<n> worktree archived by
            # an EARLIER sweep, leaving the job row dangling in the agents view.
            if [[ -n "$APPLY" && -z "$DRY_RUN" ]]; then
                _reap_jobs "__MISSING__" "$CANONICAL_MAIN"
            fi

            KEPT=$((N_DIRTY + N_UNPUSHED + N_UNMERGED + N_LIVE + N_PROC + N_SALVAGE + N_NEEDCONF))
            echo ""
            if [[ "$N_TOTAL" -eq 0 ]]; then
                echo "No non-canonical worktrees found."
            else
                EXECUTED=""; [[ -n "$APPLY" && -z "$DRY_RUN" ]] && EXECUTED="1"
                VERB="would archive"; [[ -n "$EXECUTED" ]] && VERB="archived"
                SUFFIX=""; [[ -z "$EXECUTED" ]] && SUFFIX="  [dry-run: no changes made; pass --apply to execute]"
                printf 'Summary: %d %s, %d kept (%d unmerged, %d unpushed, %d dirty, %d live-session, %d processes, %d salvage-failed, %d needs-confirmation), %d failed%s\n' \
                    "$N_REAP" "$VERB" "$KEPT" "$N_UNMERGED" "$N_UNPUSHED" "$N_DIRTY" "$N_LIVE" "$N_PROC" "$N_SALVAGE" "$N_NEEDCONF" "$N_FAIL" "$SUFFIX"
            fi
            exit 0
        fi

        REMOVED=0

        while IFS= read -r wt; do
            # Skip main repo
            [[ "$wt" == "$MAIN_DIR" ]] && continue

            # Filter by prefix if specified
            if [[ -n "$PREFIX" ]]; then
                BRANCH=$(cd "$wt" 2>/dev/null && git branch --show-current || echo "")
                [[ "$BRANCH" != ${PREFIX}* ]] && continue
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
