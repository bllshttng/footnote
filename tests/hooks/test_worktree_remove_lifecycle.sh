#!/usr/bin/env bash
# test_worktree_remove_lifecycle.sh -- guard the worktree lifecycle fixes (x-415c).
#
# Covers three subsystems that all mishandled worktree teardown:
#   1. hooks/worktree-remove.sh honors the CC WorktreeRemove delegation
#      contract (actually removes; refuses canonical; prunes already-gone;
#      refuses dirty).
#   2. scripts/lib/worktree-lifecycle.sh _wt_pids keys on process cwd, not any
#      open file, so uv-hardlinked venv .so files don't false-positive.
#   3. scripts/setup/archive-worktree.sh declines cleanly without a tty (rc=3,
#      one line, no /dev/tty spew), and the sweep reaps dead bg-job records.
#
# Bash 3.2 compatible. No network, no real claude/graph mutation.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/worktree-remove.sh"
LIFECYCLE="$REPO_ROOT/scripts/lib/worktree-lifecycle.sh"
ARCHIVE="$REPO_ROOT/scripts/setup/archive-worktree.sh"

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1 :: $2"; FAIL=$((FAIL + 1)); }

# Sandbox git repo with a hook-created worktree.
new_sandbox() {
    local tmp
    tmp=$(mktemp -d -t wt-lifecycle.XXXXXX)
    (
        cd "$tmp"
        git init -q
        # Real repos gitignore .fno/, so a manifest under a worktree never makes
        # it untracked-dirty (which would make `git worktree remove` refuse).
        printf '.fno/\n' > .gitignore
        git -c user.email=t@t -c user.name=t add .gitignore
        git -c user.email=t@t -c user.name=t commit -qm init
    ) >/dev/null 2>&1
    echo "$tmp"
}

echo "== 1. worktree-remove.sh delegation contract =="

# The hook resolves MAIN_REPO from its own cwd (as the CC harness invokes it
# from within the repo), so every invocation runs from inside the sandbox.

# 1a. Refuse the canonical checkout (exit 1, no fs change).
S=$(new_sandbox)
out=$(cd "$S" && echo "{\"worktree_path\":\"$S\"}" | bash "$HOOK" 2>&1); rc=$?
if [[ $rc -eq 1 && -d "$S/.git" ]] && echo "$out" | grep -q 'main checkout'; then pass "canonical refused (exit 1, untouched)"; else fail "canonical refuse" "rc=$rc out=$out"; fi
rm -rf "$S"

# 1b. Already-gone path -> prune + exit 0.
S=$(new_sandbox)
out=$(cd "$S" && echo "{\"worktree_path\":\"$S/never-existed\"}" | bash "$HOOK" 2>&1); rc=$?
[[ $rc -eq 0 ]] && pass "already-gone exit 0" || fail "already-gone" "rc=$rc out=$out"
rm -rf "$S"

# 1c. Merged clean hook-created worktree -> removed (exit 0, path gone).
S=$(new_sandbox)
( cd "$S" && git worktree add -q wt >/dev/null 2>&1 )
out=$(cd "$S" && echo "{\"worktree_path\":\"$S/wt\"}" | bash "$HOOK" 2>&1); rc=$?
if [[ $rc -eq 0 && ! -d "$S/wt" ]]; then pass "clean worktree removed (exit 0, gone)"; else fail "clean remove" "rc=$rc wt-exists=$([[ -d "$S/wt" ]] && echo y || echo n) out=$out"; fi
rm -rf "$S"

# 1d. Dirty worktree -> refused (exit 1, kept).
S=$(new_sandbox)
( cd "$S" && git worktree add -q wt >/dev/null 2>&1 && echo dirty > "wt/uncommitted.txt" )
out=$(cd "$S" && echo "{\"worktree_path\":\"$S/wt\"}" | bash "$HOOK" 2>&1); rc=$?
if [[ $rc -eq 1 && -d "$S/wt" ]]; then pass "dirty worktree refused (exit 1, kept)"; else fail "dirty refuse" "rc=$rc wt-exists=$([[ -d "$S/wt" ]] && echo y || echo n) out=$out"; fi
rm -rf "$S"

# 1e. Modern manifest (no status field) with a LIVE owner_pid -> preserved
# (exit 1, kept). Guards the P1 where a running claimed target's cwd would be
# removed because the status-only guard missed the modern liveness signal.
# Exit 1, not 0: the harness reads exit 0 as "removed" and deletes the job
# record, which would orphan the very worktree this branch is protecting.
# Matches the other two did-not-remove branches (main checkout, dirty).
S=$(new_sandbox)
( cd "$S" && git worktree add -q wt >/dev/null 2>&1 )
mkdir -p "$S/wt/.fno"
( exec sleep 30 ) & LIVE=$!
printf 'graph_node_id: x-live\nowner_pid: %s\n' "$LIVE" > "$S/wt/.fno/target-state.md"
out=$(cd "$S" && echo "{\"worktree_path\":\"$S/wt\"}" | bash "$HOOK" 2>&1); rc=$?
if [[ $rc -eq 1 && -d "$S/wt" ]] && echo "$out" | grep -q 'preserving'; then pass "live owner_pid (modern manifest) preserved (exit 1, kept)"; else fail "live owner_pid preserve" "rc=$rc wt-exists=$([[ -d "$S/wt" ]] && echo y || echo n) out=$out"; fi
kill "$LIVE" 2>/dev/null
rm -rf "$S"

# 1f. Modern manifest with a DEAD owner_pid -> not preserved (removed, exit 0).
S=$(new_sandbox)
( cd "$S" && git worktree add -q wt >/dev/null 2>&1 )
mkdir -p "$S/wt/.fno"
( exec true ) & DEAD=$!; wait "$DEAD" 2>/dev/null   # pid now dead
printf 'graph_node_id: x-dead\nowner_pid: %s\n' "$DEAD" > "$S/wt/.fno/target-state.md"
out=$(cd "$S" && echo "{\"worktree_path\":\"$S/wt\"}" | bash "$HOOK" 2>&1); rc=$?
if [[ $rc -eq 0 && ! -d "$S/wt" ]]; then pass "dead owner_pid not preserved (removed, exit 0)"; else fail "dead owner_pid remove" "rc=$rc wt-exists=$([[ -d "$S/wt" ]] && echo y || echo n) out=$out"; fi
rm -rf "$S"

# 1g. Unregistered leftover dir whose delete FAILS -> exit 1, remove_failed
# logged, path kept. Exit code is the whole signal: the hook must never claim
# removal while the path survives. The unwritable parent makes every rm rung
# fail. The log file exists beforehand so the append is not blocked by the
# parent's mode.
S=$(new_sandbox)
mkdir -p "$S/leftover" "$S/.fno"
: > "$S/.fno/worktree-log.jsonl"
chmod 555 "$S"
out=$(cd "$S" && echo "{\"worktree_path\":\"$S/leftover\"}" | bash "$HOOK" 2>&1); rc=$?
chmod 755 "$S"
if [[ $rc -eq 1 && -d "$S/leftover" ]] && grep -q '"action":"remove_failed"' "$S/.fno/worktree-log.jsonl" 2>/dev/null; then
    pass "failed delete exits 1 and logs remove_failed"
else
    fail "failed delete" "rc=$rc exists=$([[ -d "$S/leftover" ]] && echo y || echo n) log=$(tail -1 "$S/.fno/worktree-log.jsonl" 2>/dev/null)"
fi
rm -rf "$S"

echo "== 2. _wt_pids keys on cwd, not open files =="

# Source just the helpers (the script body runs a case statement on source).
eval "$(sed -n '/^_wt_refresh_cwd_snapshot()/,/^}/p; /^_wt_pids()/,/^}/p' "$LIFECYCLE")"

if command -v lsof >/dev/null 2>&1; then
    WT=$(mktemp -d -t wt-pids.XXXXXX); mkdir -p "$WT/sub"; echo x > "$WT/sub/f"
    # (a) process cwd'd ELSEWHERE holding an open fd under WT -> must NOT match.
    ( cd /tmp && exec 9<"$WT/sub/f"; sleep 5 ) & OFF=$!
    # (b) process cwd'd INSIDE WT -> must match.
    ( cd "$WT/sub" && exec sleep 5 ) & IN=$!
    disown "$OFF" "$IN" 2>/dev/null || true   # silence job-control "Terminated" notices
    sleep 0.6
    _wt_refresh_cwd_snapshot
    pids="$(_wt_pids "$WT")"
    if ! printf '%s\n' "$pids" | grep -qx "$OFF"; then pass "open-file-only process excluded (uv-hardlink false-positive fix)"; else fail "cwd-anchor exclude" "matched off-process $OFF"; fi
    if printf '%s\n' "$pids" | grep -qx "$IN"; then pass "cwd-inside process still detected"; else fail "cwd-anchor include" "missed in-process $IN; got [$pids]"; fi
    kill "$OFF" "$IN" 2>/dev/null
    rm -rf "$WT"
else
    echo "  SKIP: lsof unavailable"
fi

echo "== 3. archive-worktree.sh declines cleanly without a tty =="

S=$(new_sandbox)
( cd "$S" && git worktree add -q wt >/dev/null 2>&1 )
WT="$S/wt"
( cd "$WT" && exec sleep 300 ) & HOLD=$!
disown "$HOLD" 2>/dev/null || true
sleep 0.6
# Run in its OWN session (separate PGID, so the holder isn't self-filtered) with
# no controlling tty. perl provides setsid on macOS, which lacks the binary.
if command -v perl >/dev/null 2>&1; then
    out=$(perl -e 'use POSIX; setsid(); open(STDIN,"<","/dev/null"); exec @ARGV' bash "$ARCHIVE" "$WT" 2>&1); rc=$?
    [[ $rc -eq 3 ]] && pass "ttyless decline rc=3" || fail "ttyless rc" "rc=$rc"
    echo "$out" | grep -q 'no tty for confirmation' && pass "clean decline line" || fail "decline line" "$out"
    echo "$out" | grep -q 'Device not configured' && fail "no /dev/tty spew" "spew present" || pass "no /dev/tty spew"
    [[ -d "$WT" ]] && pass "worktree kept on decline" || fail "kept" "worktree removed"
else
    echo "  SKIP: perl unavailable (needed for setsid)"
fi
kill "$HOLD" 2>/dev/null
rm -rf "$S"

echo "== 4. sweep reaps dead bg-job records =="

eval "$(sed -n '/^_reap_job_candidates()/,/^}/p; /^_reap_jobs()/,/^}/p' "$LIFECYCLE")"
JH=$(mktemp -d -t reap-home.XXXXXX)
mkdir -p "$JH/.claude/jobs/jDONE" "$JH/.claude/jobs/jLIVE" "$JH/.claude/jobs/jCANON" "$JH/bin"
ARCH="/some/wt/x-abcd"; CANON="/repo/canonical"
printf '{"state":"done","cwd":"%s"}' "$ARCH"  > "$JH/.claude/jobs/jDONE/state.json"
printf '{"state":"working","cwd":"%s"}' "$ARCH" > "$JH/.claude/jobs/jLIVE/state.json"
printf '{"state":"done","cwd":"%s"}' "$CANON" > "$JH/.claude/jobs/jCANON/state.json"
cat > "$JH/bin/claude" <<'EOF'
#!/usr/bin/env bash
[[ "$1" == "rm" ]] && echo "$2" >> "$JOBS_RM_LOG"
exit 0
EOF
chmod +x "$JH/bin/claude"
(
    export HOME="$JH" PATH="$JH/bin:$PATH" JOBS_RM_LOG="$JH/rm.log"
    _reap_jobs "$ARCH" "$CANON"
) >/dev/null 2>&1
LOG="$JH/rm.log"
grep -qx jDONE  "$LOG" 2>/dev/null && pass "done job at archived path reaped (AC4)" || fail "AC4 reap" "jDONE not in log"
grep -qx jLIVE  "$LOG" 2>/dev/null && fail "live job skipped" "jLIVE reaped" || pass "live job skipped"
grep -qx jCANON "$LOG" 2>/dev/null && fail "canonical job skipped (AC4-EDGE)" "jCANON reaped" || pass "canonical job skipped (AC4-EDGE)"
rm -rf "$JH"

echo "== 5. sweep lock + O(1) ps per worktree (x-a1a5) =="

# 5a. A live-held lock from the canonical checkout makes a sweep launched from
# a linked worktree exit immediately. The common-dir location is the invariant:
# git rev-parse --show-toplevel differs across worktrees in the same repository.
S=$(new_sandbox)
git -C "$S" worktree add -q "$S/wt" >/dev/null 2>&1
COMMON=$(git -C "$S" rev-parse --git-common-dir)
case "$COMMON" in /*) ;; *) COMMON="$S/$COMMON" ;; esac
LOCKDIR="$COMMON/fno-wt-sweep.lock"
rm -rf "$LOCKDIR"; mkdir -p "$LOCKDIR"; echo $$ > "$LOCKDIR/pid"   # this test process is alive
out=$(cd "$S/wt" && bash "$LIFECYCLE" cleanup --merged --dry-run 2>&1); rc=$?
if [[ $rc -eq 0 ]] && echo "$out" | grep -q "already running" && ! echo "$out" | grep -q "^STATUS"; then
    pass "second sweep exits immediately, no scan (exit 0)"
else
    fail "concurrent sweep exclusion" "rc=$rc out=$out"
fi
rm -rf "$LOCKDIR"
rm -rf "$S"

# 5b. A lock left by a dead holder is reclaimed, not treated as live.
# --merged mode requires a fetchable origin/main; clone (not push, which a
# machine-local pre-push hook here refuses for a branch named "main") a
# bare remote from the sandbox itself so the fetch step succeeds.
S=$(new_sandbox)
git -C "$S" branch -M main >/dev/null 2>&1
BARE=$(mktemp -d -t wt-bare.XXXXXX); rmdir "$BARE"
git clone -q --bare "$S" "$BARE" >/dev/null 2>&1
git -C "$S" remote add origin "$BARE" >/dev/null 2>&1
COMMON=$(git -C "$S" rev-parse --git-common-dir)
case "$COMMON" in /*) ;; *) COMMON="$S/$COMMON" ;; esac
LOCKDIR="$COMMON/fno-wt-sweep.lock"
rm -rf "$LOCKDIR"; mkdir -p "$LOCKDIR"
( exec true ) & DEAD=$!; wait "$DEAD" 2>/dev/null   # pid now dead
echo "$DEAD" > "$LOCKDIR/pid"
out=$(cd "$S" && bash "$LIFECYCLE" cleanup --merged --dry-run 2>&1); rc=$?
if [[ $rc -eq 0 ]] && echo "$out" | grep -q "^STATUS"; then
    pass "stale lock reclaimed, sweep proceeds"
else
    fail "stale lock reclaim" "rc=$rc out=$out"
fi
rm -rf "$LOCKDIR" "$S" "$BARE"

# 5c. _wt_pids spawns exactly one `ps` snapshot per worktree, not one per
# matched pid. Stub process enumeration so the assertion stays deterministic
# inside CI sandboxes that deny access to the host process table.
eval "$(sed -n '/^_wt_refresh_cwd_snapshot()/,/^}/p; /^_wt_pids()/,/^}/p' "$LIFECYCLE")"
STUBDIR=$(mktemp -d -t ps-stub.XXXXXX)
COUNTFILE="$STUBDIR/count.log"; : > "$COUNTFILE"
cat > "$STUBDIR/lsof" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
cat > "$STUBDIR/pgrep" <<'EOF'
#!/usr/bin/env bash
printf '101\n102\n103\n104\n105\n'
EOF
cat > "$STUBDIR/ps" <<EOF
#!/usr/bin/env bash
echo \$\$ >> "$COUNTFILE"
printf '101 bash holder $STUBDIR/wt\n102 bash holder $STUBDIR/wt\n103 bash holder $STUBDIR/wt\n104 bash holder $STUBDIR/wt\n105 bash holder $STUBDIR/wt\n'
EOF
chmod +x "$STUBDIR/lsof" "$STUBDIR/pgrep" "$STUBDIR/ps"
WT="$STUBDIR/wt"; mkdir -p "$WT"
PATH="$STUBDIR:$PATH" _wt_refresh_cwd_snapshot
FOUND=$(PATH="$STUBDIR:$PATH" _wt_pids "$WT")
N_FOUND=$(printf '%s\n' "$FOUND" | grep -c .)
N_PS=$(wc -l < "$COUNTFILE" | tr -d ' ')
if [[ "$N_FOUND" -eq 5 ]]; then pass "all 5 argv-matched pids detected"; else fail "detect matches" "found $N_FOUND of 5: [$FOUND]"; fi
if [[ "$N_PS" -eq 1 ]]; then pass "exactly 1 ps call for 5 matches (was 1-per-match)"; else fail "O(1) ps calls" "ps invoked $N_PS times, want 1"; fi
rm -rf "$STUBDIR"

# 5d. pgrep can still return candidates when a sandbox denies ps. An empty
# snapshot is not evidence that the candidates are safe, so preserve them all.
STUBDIR=$(mktemp -d -t ps-empty-stub.XXXXXX)
cat > "$STUBDIR/lsof" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
cat > "$STUBDIR/pgrep" <<'EOF'
#!/usr/bin/env bash
printf '201\n202\n'
EOF
cat > "$STUBDIR/ps" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
chmod +x "$STUBDIR/lsof" "$STUBDIR/pgrep" "$STUBDIR/ps"
WT="$STUBDIR/wt"; mkdir -p "$WT"
PATH="$STUBDIR:$PATH" _wt_refresh_cwd_snapshot
FOUND=$(PATH="$STUBDIR:$PATH" _wt_pids "$WT")
if [[ "$FOUND" == $'201\n202' ]]; then
    pass "empty ps snapshot preserves all candidate pids"
else
    fail "empty ps fails closed" "want [201 202], got [$FOUND]"
fi
rm -rf "$STUBDIR"

# 5e. An unreadable cwd snapshot is not proof that a worktree is idle. The
# argv lane still returns candidates, while the nonzero status tells each
# destructive caller to keep the worktree even when argv is empty.
STUBDIR=$(mktemp -d -t cwd-unreadable-stub.XXXXXX)
cat > "$STUBDIR/lsof" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
cat > "$STUBDIR/pgrep" <<'EOF'
#!/usr/bin/env bash
printf '301\n'
EOF
cat > "$STUBDIR/ps" <<'EOF'
#!/usr/bin/env bash
printf '301 1 holder sleep 301\n'
EOF
chmod +x "$STUBDIR/lsof" "$STUBDIR/pgrep" "$STUBDIR/ps"
WT="$STUBDIR/wt"; mkdir -p "$WT"
PATH="$STUBDIR:$PATH" _wt_refresh_cwd_snapshot >/dev/null 2>&1
SNAPSHOT_RC=$?
FOUND=$(PATH="$STUBDIR:$PATH" _wt_pids "$WT")
PIDS_RC=$?
if [[ "$SNAPSHOT_RC" -ne 0 && "$PIDS_RC" -ne 0 && "$FOUND" == "301" ]]; then
    pass "unreadable cwd snapshot fails closed and preserves argv candidates"
else
    fail "unreadable cwd snapshot" "snapshot_rc=$SNAPSHOT_RC pids_rc=$PIDS_RC found=[$FOUND]"
fi
rm -rf "$STUBDIR"

# 5g. A child of this sweep carrying the sweep's own command line is the
# command-substitution subshell running _wt_pids, not an occupant (CI smoke
# 2026-09-07: pid 6327, bash <script-path>, live ps row, no cwd row). A child
# with a DIFFERENT command line stays: that is a real squatter.
STUBDIR=$(mktemp -d -t ps-subshell-stub.XXXXXX)
cat > "$STUBDIR/lsof" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
cat > "$STUBDIR/pgrep" <<'EOF'
#!/usr/bin/env bash
printf '701\n702\n'
EOF
cat > "$STUBDIR/ps" <<EOF
#!/usr/bin/env bash
printf '$$ $PPID bash $0\n'
printf '701 $$ bash $0\n'
printf '702 1 sleep 300\n'
EOF
chmod +x "$STUBDIR/lsof" "$STUBDIR/pgrep" "$STUBDIR/ps"
WT="$STUBDIR/wt"; mkdir -p "$WT"
eval "$(sed -n '/^_wt_refresh_cwd_snapshot()/,/^}/p; /^_wt_pids()/,/^}/p' "$LIFECYCLE")"
PATH="$STUBDIR:$PATH" _wt_refresh_cwd_snapshot
FOUND=$(PATH="$STUBDIR:$PATH" _wt_pids "$WT")
if [[ "$FOUND" == "702" ]]; then
    pass "sweep's own subshell dropped, real child kept"
else
    fail "subshell self-drop" "want [702], got [$FOUND]"
fi
rm -rf "$STUBDIR"

# 5f. One machine-wide cwd snapshot serves a 94-worktree phase. Field output
# keeps parsing independent of lsof's human columns; matching is exact or below
# the worktree boundary, so wt-1 never absorbs wt-10.
eval "$(sed -n '/^_wt_refresh_cwd_snapshot()/,/^}/p; /^_wt_pids()/,/^}/p; /^_cargo_target_mtime()/,/^}/p; /^_cargo_target_bytes()/,/^}/p; /^_cargo_target_inventory()/,/^}/p' "$LIFECYCLE")"
if declare -f _wt_refresh_cwd_snapshot >/dev/null 2>&1; then
    STUBDIR=$(mktemp -d -t cwd-snapshot-stub.XXXXXX)
    COUNTFILE="$STUBDIR/lsof.log"; : > "$COUNTFILE"
    WORKTREE_LIST="$STUBDIR/worktrees.txt"; : > "$WORKTREE_LIST"
    INVENTORY="$STUBDIR/inventory.tsv"
    WT_ROOT="$STUBDIR/worktrees"; mkdir -p "$WT_ROOT"
    i=1
    while [[ "$i" -le 94 ]]; do
        mkdir -p "$WT_ROOT/wt-$i/target"
        printf 'x' > "$WT_ROOT/wt-$i/target/artifact"
        printf 'worktree %s\n\n' "$WT_ROOT/wt-$i" >> "$WORKTREE_LIST"
        i=$((i + 1))
    done
    cat > "$STUBDIR/lsof" <<'EOF'
#!/usr/bin/env bash
echo "$*" >> "$COUNTFILE"
printf 'p1001\nfcwd\nn%s\np1010\nfcwd\nn%s\np1094\nfcwd\nn%s\np2000\nfcwd\nn%s\n' \
  "$WT_ROOT/wt-1" "$WT_ROOT/wt-10/sub" "$WT_ROOT/wt-94" "$WT_ROOT/elsewhere"
EOF
    cat > "$STUBDIR/pgrep" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
    cat > "$STUBDIR/ps" <<'EOF'
#!/usr/bin/env bash
printf '1001 holder\n1010 holder\n1094 holder\n2000 holder\n'
EOF
    cat > "$STUBDIR/stat" <<'EOF'
#!/usr/bin/env bash
if [[ "$1" == "-f" ]]; then
    printf 'File: fake\nType: fake\nBlocks: 1\nFree: 1\nFiles: 1\nFree files: 1\n'
    exit 0
fi
if [[ "$1" == "-c" ]]; then
    printf '1700000000\n'
    exit 0
fi
exit 1
EOF
    cat > "$STUBDIR/git" <<'EOF'
#!/usr/bin/env bash
if [[ "$*" == "worktree list --porcelain" ]]; then
    cat "$WORKTREE_LIST"
    exit 0
fi
exit 1
EOF
    chmod +x "$STUBDIR/lsof" "$STUBDIR/pgrep" "$STUBDIR/ps" "$STUBDIR/stat" "$STUBDIR/git"
    export COUNTFILE WORKTREE_LIST WT_ROOT
    _wt_live() { return 1; }
    PATH="$STUBDIR:$PATH" _cargo_target_inventory "$INVENTORY"
    N_LSOF=$(wc -l < "$COUNTFILE" | tr -d ' ')
    N_WORKTREES=$(wc -l < "$INVENTORY" | tr -d ' ')
    WT1_PROTECTION=$(awk -F '\t' -v wt="$WT_ROOT/wt-1" '$4 == wt {print $3}' "$INVENTORY")
    WT10_PROTECTION=$(awk -F '\t' -v wt="$WT_ROOT/wt-10" '$4 == wt {print $3}' "$INVENTORY")
    WT2_PROTECTION=$(awk -F '\t' -v wt="$WT_ROOT/wt-2" '$4 == wt {print $3}' "$INVENTORY")
    if [[ "$N_LSOF" -eq 1 && "$N_WORKTREES" -eq 94 && "$WT1_PROTECTION" == "processes:1" && "$WT10_PROTECTION" == "processes:1" && "$WT2_PROTECTION" == "-" ]] && ! grep -q -- '+D' "$COUNTFILE"; then
        pass "lsof_calls=1 worktrees=94"
    else
        fail "shared cwd snapshot" "lsof_calls=$N_LSOF worktrees=$N_WORKTREES wt1=$WT1_PROTECTION wt10=$WT10_PROTECTION wt2=$WT2_PROTECTION argv=[$(cat "$COUNTFILE")]"
    fi
    unset COUNTFILE WORKTREE_LIST WT_ROOT
    rm -rf "$STUBDIR"
else
    fail "shared cwd snapshot" "_wt_refresh_cwd_snapshot is missing"
fi

# 5g. Apply mode refreshes after inventory and before deletion. A process that
# appears only in the second snapshot protects the selected target.
S=$(new_sandbox)
git -C "$S" worktree add -q "$S/wt" >/dev/null 2>&1
mkdir -p "$S/wt/target"
printf 'artifact' > "$S/wt/target/file"
STUBDIR=$(mktemp -d -t cwd-recheck-stub.XXXXXX)
COUNTFILE="$STUBDIR/lsof.log"; : > "$COUNTFILE"
PROTECTED_WT="$(cd "$S/wt" && pwd -P)"
cat > "$STUBDIR/lsof" <<'EOF'
#!/usr/bin/env bash
echo "$*" >> "$COUNTFILE"
n=$(wc -l < "$COUNTFILE" | tr -d ' ')
if [[ "$n" -ge 2 ]]; then
    printf 'p401\nfcwd\nn%s\n' "$PROTECTED_WT"
fi
EOF
cat > "$STUBDIR/pgrep" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
cat > "$STUBDIR/ps" <<'EOF'
#!/usr/bin/env bash
printf '401 holder\n'
EOF
chmod +x "$STUBDIR/lsof" "$STUBDIR/pgrep" "$STUBDIR/ps"
export COUNTFILE PROTECTED_WT
out=$(cd "$S" && PATH="$STUBDIR:$PATH" bash "$LIFECYCLE" cleanup --cargo-targets --apply --cap-bytes 1 --target-max-age 0 2>&1); rc=$?
N_LSOF=$(wc -l < "$COUNTFILE" | tr -d ' ')
if [[ "$rc" -eq 1 && "$N_LSOF" -eq 3 && -d "$S/wt/target" ]] && echo "$out" | grep -q 'reason=process-recheck' && echo "$out" | grep -q 'status=over-cap-protected'; then
    pass "apply recheck refresh protects newly rooted worktree"
else
    fail "apply cwd recheck" "rc=$rc lsof_calls=$N_LSOF target_exists=$([[ -d "$S/wt/target" ]] && echo yes || echo no) out=[$out]"
fi
unset COUNTFILE PROTECTED_WT
rm -rf "$STUBDIR" "$S"

# 5h. Two sweeps racing to reclaim the same dead-holder lock: exactly one
# proceeds, the other backs off - never both, and never neither.
S=$(new_sandbox)
git -C "$S" branch -M main >/dev/null 2>&1
BARE=$(mktemp -d -t wt-bare.XXXXXX); rmdir "$BARE"
git clone -q --bare "$S" "$BARE" >/dev/null 2>&1
git -C "$S" remote add origin "$BARE" >/dev/null 2>&1
COMMON=$(git -C "$S" rev-parse --git-common-dir)
case "$COMMON" in /*) ;; *) COMMON="$S/$COMMON" ;; esac
LOCKDIR="$COMMON/fno-wt-sweep.lock"
rm -rf "$LOCKDIR"; mkdir -p "$LOCKDIR"
( exec true ) & DEAD=$!; wait "$DEAD" 2>/dev/null   # pid now dead
echo "$DEAD" > "$LOCKDIR/pid"
OUT_A=$(mktemp -t race-a.XXXXXX)
OUT_B=$(mktemp -t race-b.XXXXXX)
( cd "$S" && bash "$LIFECYCLE" cleanup --merged --dry-run >"$OUT_A" 2>&1 ) &
RACE_A=$!
( cd "$S" && bash "$LIFECYCLE" cleanup --merged --dry-run >"$OUT_B" 2>&1 ) &
RACE_B=$!
wait "$RACE_A" 2>/dev/null
wait "$RACE_B" 2>/dev/null
PROCEEDED=0
for f in "$OUT_A" "$OUT_B"; do
    grep -q "^STATUS" "$f" && PROCEEDED=$((PROCEEDED + 1))
done
if [[ "$PROCEEDED" -eq 1 ]]; then
    pass "concurrent stale-lock reclaim: exactly one sweep proceeds"
else
    fail "concurrent reclaim race" "proceeded=$PROCEEDED (want 1) A=[$(cat "$OUT_A")] B=[$(cat "$OUT_B")]"
fi
rm -f "$OUT_A" "$OUT_B"
rm -rf "$LOCKDIR" "$S" "$BARE"

# 5h2. ABA, staged deterministically - no timing, no concurrency. The
# reclaimer observes a stale lock (dead pid), and between that observation
# and the reclaim step the lock is replaced by a peer's FRESH EMPTY claim
# (the mkdir-landed, pid-not-yet-written window). The reclaimer must not eat
# what it did not observe: the fresh claim survives and the reclaimer gives
# up. A stub `cat` performs the swap inside the pid read, so the interleaving
# is forced, not raced: this test failed against the blind rmdir on main,
# failed against the mv-only reclaim, and passes against the steal that
# compares the moved directory's pid stamp against the one it observed.
S=$(new_sandbox)
git -C "$S" branch -M main >/dev/null 2>&1
BARE=$(mktemp -d -t wt-bare2.XXXXXX); rmdir "$BARE"
git clone -q --bare "$S" "$BARE" >/dev/null 2>&1
git -C "$S" remote add origin "$BARE" >/dev/null 2>&1
COMMON=$(git -C "$S" rev-parse --git-common-dir)
case "$COMMON" in /*) ;; *) COMMON="$S/$COMMON" ;; esac
# Physical path: macOS /var is a symlink to /private/var, and the script's
# own git rev-parse resolves physical, so the stub's comparison must too.
LOCKDIR="$(cd "$COMMON" && pwd -P)/fno-wt-sweep.lock"
rm -rf "$LOCKDIR"; mkdir -p "$LOCKDIR"
( exec true ) & DEAD=$!; wait "$DEAD" 2>/dev/null   # pid now dead
echo "$DEAD" > "$LOCKDIR/pid"
STUBDIR=$(mktemp -d -t aba-stub.XXXXXX)
SWAPDONE="$STUBDIR/swap-done"
cat > "$STUBDIR/cat" <<EOF
#!/usr/bin/env bash
# One-time: the FIRST pid read answers the stale pid, and the swap happens
# AFTER that read (reading first, then swapping, is the whole staging: the
# reclaimer observes the stale lock and acts on a path that now holds a
# peer's fresh empty claim). Every later read passes through, so the trap
# and the retry loop see the real world.
if [[ "\$1" == "$LOCKDIR/pid" && ! -e "$SWAPDONE" ]]; then
    _out=\$(/bin/cat "\$@")
    : > "$SWAPDONE"
    rm -rf "$LOCKDIR"
    mkdir "$LOCKDIR"
    printf '%s' "\$_out"
    exit 0
fi
exec /bin/cat "\$@"
EOF
chmod +x "$STUBDIR/cat"
OUT_ABA=$(mktemp -t aba-a.XXXXXX)
( cd "$S" && PATH="$STUBDIR:$PATH" bash "$LIFECYCLE" cleanup --merged --dry-run >"$OUT_ABA" 2>&1 )
if grep -q "^STATUS" "$OUT_ABA"; then
    fail "ABA reclaim eats a fresh claim" "reclaimer proceeded over a lock it did not observe: [$(cat "$OUT_ABA")]"
else
    pass "ABA reclaim backs off a lock it did not observe"
fi
# Positive marker for the exhausted-retry path: without it, an early
# "another sweep is already running" exit would pass the STATUS check above
# without the steal/restore logic ever running.
if grep -q "could not acquire sweep lock after retries" "$OUT_ABA"; then
    pass "ABA reclaimer exhausted retries against the fresh claim"
else
    fail "ABA reclaimer left the retry path" "no exhausted-retries marker; the sweep exited some other way"
fi
if [[ -f "$SWAPDONE" ]]; then
    pass "ABA swap instrument ran"
else
    fail "ABA swap instrument never ran" "the stub cat was never consulted; a green here would be vacuous"
fi
# The frozen pid-less claim is DEBRIS once the retry budget expires (a real
# mid-acquire peer writes its pid in milliseconds), and the budget-expiry
# reap must leave the next sweep a clean path: a wedge here is permanent.
OUT_ABA_B=$(mktemp -t aba-b.XXXXXX)
( cd "$S" && bash "$LIFECYCLE" cleanup --merged --dry-run >"$OUT_ABA_B" 2>&1 )
if grep -q "^STATUS" "$OUT_ABA_B"; then
    pass "ABA debris does not wedge the next sweep"
else
    fail "ABA debris wedged the next sweep" "second sweep could not acquire: [$(cat "$OUT_ABA_B")]"
fi
rm -f "$OUT_ABA" "$OUT_ABA_B"
rm -rf "$STUBDIR" "$LOCKDIR" "$S" "$BARE"

# 5h3. The else arm: the path is re-taken before the restore, the moved copy
# carries a LIVE stamp, and the reclaimer must LEAVE it (deleting a live
# claim is the eat by another name). Staged with a stub `mv` that performs
# the real steal and then plants a third party's claim on the path, and a
# swap-in directory whose stamp names pid 1 (alive on every platform this
# suite targets).
S=$(new_sandbox)
git -C "$S" branch -M main >/dev/null 2>&1
BARE=$(mktemp -d -t wt-bare3.XXXXXX); rmdir "$BARE"
git clone -q --bare "$S" "$BARE" >/dev/null 2>&1
git -C "$S" remote add origin "$BARE" >/dev/null 2>&1
COMMON=$(git -C "$S" rev-parse --git-common-dir)
case "$COMMON" in /*) ;; *) COMMON="$S/$COMMON" ;; esac
LOCKDIR="$(cd "$COMMON" && pwd -P)/fno-wt-sweep.lock"
rm -rf "$LOCKDIR"; mkdir -p "$LOCKDIR"
( exec true ) & DEAD=$!; wait "$DEAD" 2>/dev/null
echo "$DEAD" > "$LOCKDIR/pid"
STUBDIR=$(mktemp -d -t aba3-stub.XXXXXX)
SWAPDONE="$STUBDIR/swap-done"
MVDONE="$STUBDIR/mv-done"
cat > "$STUBDIR/cat" <<EOF
#!/usr/bin/env bash
# One-time stale-pid answer, then the swap-in carries a LIVE stamp: this
# test's own pid, same user, visible to kill -0. pid 1 would LIE here - as
# root-owned launchd/systemd it answers EPERM to a user's kill -0 and reads
# as dead.
if [[ "\$1" == "$LOCKDIR/pid" && ! -e "$SWAPDONE" ]]; then
    _out=\$(/bin/cat "\$@")
    : > "$SWAPDONE"
    rm -rf "$LOCKDIR"
    mkdir "$LOCKDIR"
    echo $$ > "$LOCKDIR/pid"
    printf '%s' "\$_out"
    exit 0
fi
exec /bin/cat "\$@"
EOF
cat > "$STUBDIR/mv" <<EOF
#!/usr/bin/env bash
# One-time: perform the real steal, then take the path with a third party's
# claim so the restore finds the path occupied and the else arm is reached.
if [[ "\$1" == "$LOCKDIR" && ! -e "$MVDONE" ]]; then
    : > "$MVDONE"
    /bin/mv "\$@"
    mkdir "$LOCKDIR"
    exit 0
fi
exec /bin/mv "\$@"
EOF
chmod +x "$STUBDIR/cat" "$STUBDIR/mv"
OUT_ABA3=$(mktemp -t aba3.XXXXXX)
( cd "$S" && PATH="$STUBDIR:$PATH" bash "$LIFECYCLE" cleanup --merged --dry-run >"$OUT_ABA3" 2>&1 )
if grep -q "^STATUS" "$OUT_ABA3"; then
    fail "ABA3 live-claim copy eaten" "reclaimer proceeded over an unobserved live claim: [$(cat "$OUT_ABA3")]"
else
    pass "ABA3 reclaimer backs off a live-stamp claim"
fi
if ls "$LOCKDIR".stale.* >/dev/null 2>&1; then
    pass "ABA3 live-stamp copy left in place"
else
    fail "ABA3 live-stamp copy deleted" "the else arm reaped a claim whose stamp names a live process"
fi
if [[ -f "$MVDONE" && -f "$SWAPDONE" ]]; then
    pass "ABA3 instruments ran"
else
    fail "ABA3 instruments never ran" "the stubs were never consulted; a green here would be vacuous"
fi
rm -f "$OUT_ABA3"
rm -rf "$STUBDIR" "$LOCKDIR" "$S" "$BARE" "$LOCKDIR".stale.* 2>/dev/null

# 5h4. A holder whose verify read lies must recover its own claim, not back
# off to itself. The stub `cat` answers the FIRST pid read - the holder's own
# post-mkdir verify - with a foreign pid, once. The holder must loop, see
# ITS OWN live pid on attempt two, reclaim its own directory via the self
# arm, and acquire on attempt three. Without the self arm the sweep reads
# its own pid as "another sweep is already running" and exits, stranding a
# live-pid lock that only a later sweep can reclaim.
S=$(new_sandbox)
git -C "$S" branch -M main >/dev/null 2>&1
BARE=$(mktemp -d -t wt-bare4.XXXXXX); rmdir "$BARE"
git clone -q --bare "$S" "$BARE" >/dev/null 2>&1
git -C "$S" remote add origin "$BARE" >/dev/null 2>&1
COMMON=$(git -C "$S" rev-parse --git-common-dir)
case "$COMMON" in /*) ;; *) COMMON="$S/$COMMON" ;; esac
LOCKDIR="$(cd "$COMMON" && pwd -P)/fno-wt-sweep.lock"
rm -rf "$LOCKDIR"
STUBDIR=$(mktemp -d -t aba4-stub.XXXXXX)
LIEDONE="$STUBDIR/lie-done"
cat > "$STUBDIR/cat" <<EOF
#!/usr/bin/env bash
# The FIRST pid read is the holder's own verify (the path starts clear, so
# no earlier read exists). Answer it with a foreign pid, once; every later
# read passes through.
if [[ "\$1" == "$LOCKDIR/pid" && ! -e "$LIEDONE" ]]; then
    : > "$LIEDONE"
    printf '999'
    exit 0
fi
exec /bin/cat "\$@"
EOF
chmod +x "$STUBDIR/cat"
OUT_ABA4=$(mktemp -t aba4.XXXXXX)
( cd "$S" && PATH="$STUBDIR:$PATH" bash "$LIFECYCLE" cleanup --merged --dry-run >"$OUT_ABA4" 2>&1 )
if grep -q "^STATUS" "$OUT_ABA4"; then
    pass "verify-loop holder recovers its own lost claim"
else
    fail "verify-loop holder backed off to itself" "[$(tail -1 "$OUT_ABA4")]"
fi
if [[ -f "$LIEDONE" ]]; then
    pass "verify instrument ran"
else
    fail "verify instrument never ran" "the stub cat was never consulted; a green here would be vacuous"
fi
rm -f "$OUT_ABA4"
rm -rf "$STUBDIR" "$LOCKDIR" "$S" "$BARE"

# 5h5. A restore that lands NESTED inside a claim which took the path in
# front of the mv must lift our copy back out, leaving the holder able to
# rmdir its own directory later. Staged with a stub `mv` that passes the
# steal through, then on the restore plants a third party's directory before
# performing the real mv, which moves ours INSIDE theirs. The swap-in carries
# this test's pid as its stamp, so the moved copy mismatches the observed
# dead pid and the restore arm is the one that runs.
S=$(new_sandbox)
git -C "$S" branch -M main >/dev/null 2>&1
BARE=$(mktemp -d -t wt-bare5.XXXXXX); rmdir "$BARE"
git clone -q --bare "$S" "$BARE" >/dev/null 2>&1
git -C "$S" remote add origin "$BARE" >/dev/null 2>&1
COMMON=$(git -C "$S" rev-parse --git-common-dir)
case "$COMMON" in /*) ;; *) COMMON="$S/$COMMON" ;; esac
LOCKDIR="$(cd "$COMMON" && pwd -P)/fno-wt-sweep.lock"
rm -rf "$LOCKDIR"; mkdir -p "$LOCKDIR"
( exec true ) & DEAD=$!; wait "$DEAD" 2>/dev/null
echo "$DEAD" > "$LOCKDIR/pid"
STUBDIR=$(mktemp -d -t aba5-stub.XXXXXX)
SWAPDONE="$STUBDIR/swap-done"
MVDONE="$STUBDIR/mv-done"
cat > "$STUBDIR/cat" <<EOF
#!/usr/bin/env bash
# One-time stale-pid answer, then swap the lock for a claim whose stamp is
# THIS test's pid: alive, and different from the observed dead pid, forcing
# the mismatch that takes the restore arm.
if [[ "\$1" == "$LOCKDIR/pid" && ! -e "$SWAPDONE" ]]; then
    _out=\$(/bin/cat "\$@")
    : > "$SWAPDONE"
    rm -rf "$LOCKDIR"
    mkdir "$LOCKDIR"
    echo $$ > "$LOCKDIR/pid"
    printf '%s' "\$_out"
    exit 0
fi
exec /bin/cat "\$@"
EOF
cat > "$STUBDIR/mv" <<EOF
#!/usr/bin/env bash
# First call is the steal (pass it through). Second call is the restore:
# plant a third party's claim on the path BEFORE the real mv, so the mv
# nests our copy inside theirs.
if [[ -n "\$2" && "\$2" == "$LOCKDIR" && -e "$SWAPDONE" && ! -e "$MVDONE" ]]; then
    : > "$MVDONE"
    mkdir "$LOCKDIR"
    /bin/mv "\$@"
    exit 0
fi
exec /bin/mv "\$@"
EOF
chmod +x "$STUBDIR/cat" "$STUBDIR/mv"
OUT_ABA5=$(mktemp -t aba5.XXXXXX)
( cd "$S" && PATH="$STUBDIR:$PATH" bash "$LIFECYCLE" cleanup --merged --dry-run >"$OUT_ABA5" 2>&1 )
if grep -q "^STATUS" "$OUT_ABA5"; then
    fail "ABA5 reclaimer proceeded after a nested restore" "[$(cat "$OUT_ABA5")]"
else
    pass "ABA5 backs off after a nested restore"
fi
if ls "$LOCKDIR"/fno-wt-sweep.lock.stale.* >/dev/null 2>&1; then
    fail "ABA5 nested copy never lifted" "our stale copy is still nested inside the holder's directory"
else
    pass "ABA5 nested copy lifted out of the holder's directory"
fi
if [[ -f "$MVDONE" && -f "$SWAPDONE" ]]; then
    pass "ABA5 instruments ran"
else
    fail "ABA5 instruments never ran" "the stubs were never consulted; a green here would be vacuous"
fi
rm -f "$OUT_ABA5"
rm -rf "$STUBDIR" "$LOCKDIR" "$S" "$BARE" "$LOCKDIR".stale.* 2>/dev/null

# 5h6. The acquisition-time sibling sweep reaps stale-steal leftovers: a
# sibling whose stamp is absent or dead goes, one whose stamp names a live
# process stays. No stubs: two leftovers are planted by hand and a normal
# sweep does the reaping.
S=$(new_sandbox)
git -C "$S" branch -M main >/dev/null 2>&1
BARE=$(mktemp -d -t wt-bare6.XXXXXX); rmdir "$BARE"
git clone -q --bare "$S" "$BARE" >/dev/null 2>&1
git -C "$S" remote add origin "$BARE" >/dev/null 2>&1
COMMON=$(git -C "$S" rev-parse --git-common-dir)
case "$COMMON" in /*) ;; *) COMMON="$S/$COMMON" ;; esac
LOCKDIR="$(cd "$COMMON" && pwd -P)/fno-wt-sweep.lock"
rm -rf "$LOCKDIR"
( exec true ) & DEAD=$!; wait "$DEAD" 2>/dev/null
mkdir -p "$LOCKDIR.stale.999001"; echo "$DEAD" > "$LOCKDIR.stale.999001/pid"
mkdir -p "$LOCKDIR.stale.999002"; echo "$$" > "$LOCKDIR.stale.999002/pid"
OUT_ABA6=$(mktemp -t aba6.XXXXXX)
( cd "$S" && bash "$LIFECYCLE" cleanup --merged --dry-run >"$OUT_ABA6" 2>&1 )
if grep -q "^STATUS" "$OUT_ABA6"; then
    pass "sibling GC sweep acquired normally"
else
    fail "sibling GC sweep never acquired" "[$(tail -1 "$OUT_ABA6")]"
fi
if [[ ! -d "$LOCKDIR.stale.999001" ]]; then
    pass "sibling GC reaped the dead-stamp leftover"
else
    fail "sibling GC kept a dead-stamp leftover" "the interrupted-steal debris was never reaped"
fi
if [[ -d "$LOCKDIR.stale.999002" ]]; then
    pass "sibling GC left the live-stamp leftover"
else
    fail "sibling GC ate a live-stamp leftover" "a claim naming a live process was reaped"
fi
rm -f "$OUT_ABA6"
rm -rf "$STUBDIR" "$LOCKDIR" "$S" "$BARE" "$LOCKDIR".stale.* 2>/dev/null

# 5h7. The deepest wedge: a PID-LESS dir WITH content (an interrupted steal's
# nested copy, whose owner's trap unlinked the pid but could not rmdir) is
# invisible to the empty-only reap and used to block every future sweep
# forever. The budget-expiry grace reaps it once it is older than the sweep
# itself (find -newer against the sweep's birth certificate), so sweep one
# gives up and sweep two acquires. No stubs; two ordinary sweeps.
S=$(new_sandbox)
git -C "$S" branch -M main >/dev/null 2>&1
BARE=$(mktemp -d -t wt-bare7.XXXXXX); rmdir "$BARE"
git clone -q --bare "$S" "$BARE" >/dev/null 2>&1
git -C "$S" remote add origin "$BARE" >/dev/null 2>&1
COMMON=$(git -C "$S" rev-parse --git-common-dir)
case "$COMMON" in /*) ;; *) COMMON="$S/$COMMON" ;; esac
LOCKDIR="$(cd "$COMMON" && pwd -P)/fno-wt-sweep.lock"
rm -rf "$LOCKDIR"; mkdir -p "$LOCKDIR"
printf 'junk' > "$LOCKDIR/debris"   # pid-less AND non-empty: the wedge shape
OUT_W1=$(mktemp -t w7a.XXXXXX)
OUT_W2=$(mktemp -t w7b.XXXXXX)
( cd "$S" && bash "$LIFECYCLE" cleanup --merged --dry-run >"$OUT_W1" 2>&1 )
if grep -q "^STATUS" "$OUT_W1"; then
    fail "wedge sweep one acquired over debris" "[$(cat "$OUT_W1")]"
else
    pass "wedge sweep one gives up on the debris dir"
fi
if [[ ! -d "$LOCKDIR" ]]; then
    pass "wedge debris reaped at budget expiry"
else
    fail "wedge debris survived the grace" "the pid-less non-empty dir still blocks the path"
fi
( cd "$S" && bash "$LIFECYCLE" cleanup --merged --dry-run >"$OUT_W2" 2>&1 )
if grep -q "^STATUS" "$OUT_W2"; then
    pass "wedge does not reach the second sweep"
else
    fail "wedge blocked the second sweep" "[$(tail -1 "$OUT_W2")]"
fi
rm -f "$OUT_W1" "$OUT_W2"
rm -rf "$LOCKDIR" "$S" "$BARE"

# 5h8. The empty-only reap's guard, pinned: a peer's pid write landing
# between the reaper's emptiness read and the rmdir makes the dir non-empty,
# the rmdir fails, and the reaper backs off - it never removes a holder's
# directory. Staged with a stub `ls` that reports the dir EMPTY once while
# planting a live peer's pid file inside it.
S=$(new_sandbox)
git -C "$S" branch -M main >/dev/null 2>&1
BARE=$(mktemp -d -t wt-bare8.XXXXXX); rmdir "$BARE"
git clone -q --bare "$S" "$BARE" >/dev/null 2>&1
git -C "$S" remote add origin "$BARE" >/dev/null 2>&1
COMMON=$(git -C "$S" rev-parse --git-common-dir)
case "$COMMON" in /*) ;; *) COMMON="$S/$COMMON" ;; esac
LOCKDIR="$(cd "$COMMON" && pwd -P)/fno-wt-sweep.lock"
rm -rf "$LOCKDIR"; mkdir -p "$LOCKDIR"
STUBDIR=$(mktemp -d -t aba8-stub.XXXXXX)
PEERPID="$LOCKDIR/pid"
LSDONE="$STUBDIR/ls-done"
cat > "$STUBDIR/ls" <<EOF
#!/usr/bin/env bash
# One-time: the reaper's emptiness read answers EMPTY while a live peer's
# pid file lands in the directory, so the rmdir must fail and the reaper
# must back off rather than reap a holder.
if [[ "\$1" == "-A" && "\$2" == "$LOCKDIR" && ! -e "$LSDONE" ]]; then
    : > "$LSDONE"
    echo $$ > "$PEERPID"
    exit 0
fi
exec /bin/ls "\$@"
EOF
chmod +x "$STUBDIR/ls"
OUT_W3=$(mktemp -t w8.XXXXXX)
( cd "$S" && PATH="$STUBDIR:$PATH" bash "$LIFECYCLE" cleanup --merged --dry-run >"$OUT_W3" 2>&1 )
if grep -q "^STATUS" "$OUT_W3"; then
    fail "reaper removed a holder's directory" "[$(cat "$OUT_W3")]"
else
    pass "reaper backs off when the dir grew a holder"
fi
if [[ -f "$PEERPID" ]]; then
    pass "peer holder survives the reap attempt"
else
    fail "peer holder was removed" "the pid write that landed first did not protect the directory"
fi
if [[ -f "$LSDONE" ]]; then
    pass "reap instrument ran"
else
    fail "reap instrument never ran" "the stub ls was never consulted; a green here would be vacuous"
fi
rm -f "$OUT_W3"
rm -rf "$STUBDIR" "$LOCKDIR" "$S" "$BARE"

echo "== 6. the sweep lock resolves its own directory, honestly =="

# 6a. The regression: from a subdirectory the lock must still land in the
# real common dir and the sweep must produce its normal listing. The lock
# path is `git rev-parse --path-format=absolute --git-common-dir`; a join of
# the raw relative answer onto the toplevel only holds at the toplevel.
S=$(new_sandbox)
git -C "$S" branch -M main >/dev/null 2>&1
BARE=$(mktemp -d -t wt-bare6.XXXXXX); rmdir "$BARE"
git clone -q --bare "$S" "$BARE" >/dev/null 2>&1
git -C "$S" remote add origin "$BARE" >/dev/null 2>&1
mkdir -p "$S/sub"
OUT_SUB=$(mktemp -t sub-out.XXXXXX)
ERR_SUB=$(mktemp -t sub-err.XXXXXX)
( cd "$S/sub" && bash "$LIFECYCLE" cleanup --merged --dry-run >"$OUT_SUB" 2>"$ERR_SUB" )
if grep -q "^STATUS" "$OUT_SUB" && ! grep -q "could not acquire sweep lock" "$ERR_SUB" \
    && ! grep -q "No such file or directory" "$ERR_SUB"; then
    pass "sweep runs from a subdirectory, no lock error"
else
    fail "sweep from subdirectory" "out=[$(cat "$OUT_SUB")] err=[$(cat "$ERR_SUB")]"
fi
rm -f "$OUT_SUB" "$ERR_SUB"
rm -rf "$S" "$BARE"

# 6b. An unusable lock directory refuses honestly: non-zero exit, and the
# message names the real cause instead of calling it contention. A caller
# who reads "after retries" waits out a race that does not exist.
NONREPO=$(mktemp -d -t wt-nonrepo.XXXXXX)
OUT_NR=$(mktemp -t nr-out.XXXXXX)
ERR_NR=$(mktemp -t nr-err.XXXXXX)
( cd "$NONREPO" && bash "$LIFECYCLE" cleanup --merged --dry-run >"$OUT_NR" 2>"$ERR_NR" ); rc=$?
if [[ "$rc" -ne 0 ]] && grep -q "not lock contention" "$ERR_NR" && ! grep -q "after retries" "$ERR_NR"; then
    pass "unusable lock dir refuses honestly"
else
    fail "unusable lock dir refusal" "rc=$rc err=[$(cat "$ERR_NR")]"
fi
rm -f "$OUT_NR" "$ERR_NR"
rm -rf "$NONREPO"

# 6c. Genuine contention keeps its honest shape: a live holder is reported
# by pid and exits 0, so the honest refusal above can never collapse every
# lock outcome into one message.
S=$(new_sandbox)
git -C "$S" branch -M main >/dev/null 2>&1
BARE=$(mktemp -d -t wt-bare6c.XXXXXX); rmdir "$BARE"
git clone -q --bare "$S" "$BARE" >/dev/null 2>&1
git -C "$S" remote add origin "$BARE" >/dev/null 2>&1
COMMON=$(git -C "$S" rev-parse --git-common-dir)
case "$COMMON" in /*) ;; *) COMMON="$S/$COMMON" ;; esac
LOCKDIR="$COMMON/fno-wt-sweep.lock"
rm -rf "$LOCKDIR"; mkdir -p "$LOCKDIR"
echo "$$" > "$LOCKDIR/pid"   # this test's own pid: alive for the whole run
OUT_C=$(mktemp -t cont-out.XXXXXX)
ERR_C=$(mktemp -t cont-err.XXXXXX)
( cd "$S" && bash "$LIFECYCLE" cleanup --merged --dry-run >"$OUT_C" 2>"$ERR_C" ); rc=$?
if [[ "$rc" -eq 0 ]] && grep -q "another sweep (pid" "$ERR_C"; then
    pass "genuine contention still reports pid, exit 0"
else
    fail "genuine contention shape" "rc=$rc err=[$(cat "$ERR_C")]"
fi
rm -f "$OUT_C" "$ERR_C"
rm -rf "$LOCKDIR" "$S" "$BARE"

echo "== 7. cleanup --merged consumes the occupancy classifier (x-0396) =="

# A merged, pushed, clean sandbox tree: branch tip reachable from origin/main,
# nothing unpushed, so step 4 (processes) is the first guard the tree meets.
# --no-verify: a machine-local pre-push hook refuses branch main on some
# operator boxes (see section 5b); the fixture is throwaway, the hook is not
# under test.
new_merged_tree() {
    local S BARE
    S=$(new_sandbox)
    git -C "$S" branch -M main >/dev/null 2>&1
    BARE=$(mktemp -d -t wt-occ-bare.XXXXXX); rmdir "$BARE"
    git clone -q --bare "$S" "$BARE" >/dev/null 2>&1
    git -C "$S" remote add origin "$BARE"
    git -C "$S" push -q --no-verify origin main >/dev/null 2>&1
    git -C "$S" worktree add -q -b feature/occ "$S/wt" >/dev/null 2>&1
    git -C "$S/wt" -c user.email=t@t -c user.name=t commit -q --allow-empty -m wip >/dev/null 2>&1
    git -C "$S/wt" push -q --no-verify -u origin feature/occ >/dev/null 2>&1
    git -C "$S" -c user.email=t@t -c user.name=t merge -q -m merge origin/feature/occ >/dev/null 2>&1
    git -C "$S" push -q --no-verify origin main >/dev/null 2>&1
    printf '%s\n' "$S"
}

# Stub classifier: rows driven by OCC_STUB_MODE, calls logged to OCC_STUB_LOG.
make_occupancy_stub() {
    local dir="$1"
    mkdir -p "$dir"
    cat > "$dir/classify" <<'EOF'
#!/usr/bin/env bash
wt="$1"; shift
printf 'called %s %s\n' "$wt" "$*" >> "$OCC_STUB_LOG"
mode="${OCC_STUB_MODE:-inert}"
if [[ "$mode" == "exit2" ]]; then exit 2; fi
if [[ "$mode" == "short" ]]; then
  printf '%s\tinert\tterminate\t-\tstub\tstub\n' "$1"
  exit 0
fi
first=1
for p in "$@"; do
  m="$mode"
  if [[ "$mode" == "mixed" ]]; then
    if [[ $first -eq 1 ]]; then m="holds"; else m="inert"; fi
    first=0
  fi
  if [[ "$mode" == "flip" ]]; then
    n="$(wc -l < "$OCC_STUB_LOG" | tr -d ' ')"
    if [[ "$n" -gt 1 ]]; then m="holds"; else m="inert"; fi
  fi
  if [[ "$m" == "holds" ]]; then
    printf '%s\tholds\tkeep\t-\tstub holder\tsleep 300\n' "$p"
  else
    printf '%s\tinert\tterminate\t-\tstub inert\tsleep 300\n' "$p"
  fi
done
exit 0
EOF
    chmod +x "$dir/classify"
}

# 6a. AC8-HP: all-inert stub releases the tree in dry run: would-archive plus
# one indented row per pid, Summary counts it under "would archive".
S=$(new_merged_tree)
STUB=$(mktemp -d -t occ-stub.XXXXXX)
OCCLOG="$STUB/calls.log"; : > "$OCCLOG"
make_occupancy_stub "$STUB"
( cd "$S/wt" && exec sleep 300 ) & HOLD=$!
disown "$HOLD" 2>/dev/null || true
sleep 0.6
out=$(cd "$S" && OCC_STUB_LOG="$OCCLOG" FNO_WT_OCCUPANCY_CMD="$STUB/classify" bash "$LIFECYCLE" cleanup --merged --dry-run 2>&1); rc=$?
if [[ $rc -eq 0 ]] && echo "$out" | grep -q "would-archive"; then
    pass "all-inert tree prints would-archive (AC8)"
else
    fail "AC8 would-archive" "rc=$rc out=[$out]"
fi
if echo "$out" | grep -q "    $HOLD inert stub inert | sleep 300"; then
    pass "would-archive carries the indented per-pid row (AC8)"
else
    fail "AC8 indented row" "pid $HOLD missing from [$out]"
fi
if echo "$out" | grep -q "^Summary: 1 would archive" && ! echo "$out" | grep -qE "^Summary:.*[1-9] processes"; then
    pass "Summary counts the tree under would archive, not processes (AC8)"
else
    fail "AC8 summary" "[$(echo "$out" | grep '^Summary:')]"
fi

# 6b. AC9-HP: one holds + one inert -> kept with both rows, counted as processes.
S=$(new_merged_tree)
OCCLOG="$STUB/mixed.log"; : > "$OCCLOG"
( cd "$S/wt" && exec sleep 301 ) & HOLD1=$!
( cd "$S/wt" && exec sleep 302 ) & HOLD2=$!
disown "$HOLD1" "$HOLD2" 2>/dev/null || true
sleep 0.6
out=$(cd "$S" && OCC_STUB_LOG="$OCCLOG" OCC_STUB_MODE=mixed FNO_WT_OCCUPANCY_CMD="$STUB/classify" bash "$LIFECYCLE" cleanup --merged --dry-run 2>&1); rc=$?
if [[ $rc -eq 0 ]] && echo "$out" | grep -q "kept (processes: 1 held, 1 inert)"; then
    pass "mixed verdicts print the held/inert receipt (AC9)"
else
    fail "AC9 receipt" "rc=$rc out=[$out]"
fi
if echo "$out" | grep -q "stub holder" && echo "$out" | grep -q "stub inert"; then
    pass "both rows named beside the kept tree (AC9)"
else
    fail "AC9 rows" "[$out]"
fi
echo "$out" | grep -qE "^Summary:.* 1 processes" && pass "Summary processes count includes the held tree (AC9)" || fail "AC9 summary" "[$(echo "$out" | grep '^Summary:')]"

# 6c. AC10-EDGE: stub exits 2, or prints fewer rows than pids -> fail closed.
for badmode in exit2 short; do
    S=$(new_merged_tree)
    OCCLOG="$STUB/$badmode.log"; : > "$OCCLOG"
    ( cd "$S/wt" && exec sleep 303 ) & HOLD3=$!
    ( cd "$S/wt" && exec sleep 304 ) & HOLD4=$!
    disown "$HOLD3" "$HOLD4" 2>/dev/null || true
    sleep 0.6
    out=$(cd "$S" && OCC_STUB_LOG="$OCCLOG" OCC_STUB_MODE=$badmode FNO_WT_OCCUPANCY_CMD="$STUB/classify" bash "$LIFECYCLE" cleanup --merged --dry-run 2>&1); rc=$?
    if [[ $rc -eq 0 ]] && echo "$out" | grep -q "kept (processes: 2 held, 0 inert)" && echo "$out" | grep -q "classifier unavailable"; then
        pass "$badmode stub fails closed to holds (AC10)"
    else
        fail "AC10 $badmode" "rc=$rc out=[$out]"
    fi
    kill "$HOLD3" "$HOLD4" 2>/dev/null
    rm -rf "$S"
done

# 6d. AC11-EDGE: an unpushed tree is decided at step 2; the stub never runs.
S=$(new_sandbox)
git -C "$S" branch -M main >/dev/null 2>&1
BARE11=$(mktemp -d -t wt-occ-bare.XXXXXX); rmdir "$BARE11"
git clone -q --bare "$S" "$BARE11" >/dev/null 2>&1
git -C "$S" remote add origin "$BARE11"
git -C "$S" worktree add -q -b feature/unpushed "$S/wt" >/dev/null 2>&1
( cd "$S/wt" && echo x > f.txt && git -c user.email=t@t -c user.name=t add f.txt && git -c user.email=t@t -c user.name=t commit -qm wip ) >/dev/null 2>&1
OCCLOG="$STUB/unpushed.log"; : > "$OCCLOG"
out=$(cd "$S" && OCC_STUB_LOG="$OCCLOG" FNO_WT_OCCUPANCY_CMD="$STUB/classify" bash "$LIFECYCLE" cleanup --merged --dry-run 2>&1); rc=$?
if [[ $rc -eq 0 ]] && echo "$out" | grep -q "kept (unpushed)" && [[ ! -s "$OCCLOG" ]]; then
    pass "unpushed tree kept before the classifier is consulted (AC11)"
else
    fail "AC11 unpushed-first" "rc=$rc log=[$(cat "$OCCLOG")] out=[$out]"
fi
rm -rf "$S"

# 6e. AC12-HP: --kill-orphans is retired: one stderr line, verdicts unchanged.
S=$(new_merged_tree)
OCCLOG="$STUB/retired.log"; : > "$OCCLOG"
( cd "$S/wt" && exec sleep 305 ) & HOLD5=$!
disown "$HOLD5" 2>/dev/null || true
sleep 0.6
out=$(cd "$S" && OCC_STUB_LOG="$OCCLOG" FNO_WT_OCCUPANCY_CMD="$STUB/classify" bash "$LIFECYCLE" cleanup --merged --dry-run 2>&1); rc1=$?
out_flag=$(cd "$S" && OCC_STUB_LOG="$OCCLOG" FNO_WT_OCCUPANCY_CMD="$STUB/classify" bash "$LIFECYCLE" cleanup --merged --dry-run --kill-orphans 2>&1); rc2=$?
if [[ $rc2 -eq 0 ]] && echo "$out_flag" | grep -q -- "--kill-orphans is retired"; then
    pass "retirement line on stderr (AC12)"
else
    fail "AC12 retirement line" "rc=$rc2 out=[$out_flag]"
fi
if [[ "$rc1" -eq 0 ]] && [[ "$(echo "$out" | grep -c 'would-archive')" -eq 1 ]] && [[ "$(echo "$out_flag" | grep -c 'would-archive')" -eq 1 ]]; then
    pass "verdicts equal a run without the flag (AC12)"
else
    fail "AC12 verdict parity" "plain=[$out] flagged=[$out_flag]"
fi
kill "$HOLD5" 2>/dev/null
rm -rf "$S"
rm -rf "$STUB"

echo "== 8. archive-worktree.sh classifies at removal time (x-0396) =="

# 7a. AC13-HP: all re-enumerated pids inert terminate -> signalled exactly,
# removal proceeds headless with no exit 3. The holder runs in its OWN
# session (perl setsid) so the archive's self-PGID filter cannot drop it.
S=$(new_merged_tree)
STUB=$(mktemp -d -t occ-stub2.XXXXXX)
OCCLOG="$STUB/ac13.log"; : > "$OCCLOG"
make_occupancy_stub "$STUB"
( cd "$S/wt" && exec perl -e 'use POSIX; setsid(); exec "sleep", "300"' ) & HOLD=$!
disown "$HOLD" 2>/dev/null || true
sleep 0.6
out=$(perl -e 'use POSIX; setsid(); open(STDIN,"<","/dev/null"); exec @ARGV' env OCC_STUB_LOG="$OCCLOG" FNO_WT_OCCUPANCY_CMD="$STUB/classify" bash "$ARCHIVE" "$S/wt" 2>&1); rc=$?
if [[ $rc -eq 0 && ! -d "$S/wt" ]]; then
    pass "all-inert tree archives headless (AC13)"
else
    fail "AC13 archive" "rc=$rc exists=$([[ -d "$S/wt" ]] && echo y || echo n) out=[$out]"
fi
sleep 1
if kill -0 "$HOLD" 2>/dev/null; then
    fail "AC13 signal" "terminate pid $HOLD survived"
else
    pass "terminate pid signalled (AC13)"
fi
rm -rf "$S"

# 7b. AC14-EDGE: one re-enumerated pid holds -> exit 3 headless, nothing signalled.
S=$(new_merged_tree)
OCCLOG="$STUB/ac14.log"; : > "$OCCLOG"
( cd "$S/wt" && exec perl -e 'use POSIX; setsid(); exec "sleep", "300"' ) & HOLD=$!
disown "$HOLD" 2>/dev/null || true
sleep 0.6
out=$(perl -e 'use POSIX; setsid(); open(STDIN,"<","/dev/null"); exec @ARGV' env OCC_STUB_MODE=holds OCC_STUB_LOG="$OCCLOG" FNO_WT_OCCUPANCY_CMD="$STUB/classify" bash "$ARCHIVE" "$S/wt" 2>&1); rc=$?
if [[ $rc -eq 3 && -d "$S/wt" ]] && echo "$out" | grep -q 'no tty for confirmation'; then
    pass "holds row keeps the headless decline (AC14)"
else
    fail "AC14 decline" "rc=$rc exists=$([[ -d "$S/wt" ]] && echo y || echo n) out=[$out]"
fi
if kill -0 "$HOLD" 2>/dev/null; then
    pass "no pid signalled on the holds path (AC14)"
else
    fail "AC14 no-signal" "holder $HOLD was signalled"
fi
kill "$HOLD" 2>/dev/null
rm -rf "$S"

# 7c. AC14 sweep combo: the removal-time re-read overrules the sweep's older
# read - a process that turned holds between step 4 and archive keeps the tree.
# The sweep resolves archive-worktree.sh from ITS OWN repo root, so the fixture
# carries a copy of the script and its lib deps.
S=$(new_merged_tree)
mkdir -p "$S/scripts/setup" "$S/scripts/lib"
cp "$REPO_ROOT/scripts/setup/archive-worktree.sh" "$S/scripts/setup/"
cp "$REPO_ROOT/scripts/lib/worktree-reapable.sh" \
   "$REPO_ROOT/scripts/lib/worktree-unpushed.sh" \
   "$REPO_ROOT/scripts/lib/worktree-removal-event.sh" \
   "$REPO_ROOT/scripts/lib/worktree-occupancy.sh" \
   "$S/scripts/lib/"
OCCLOG="$STUB/flip.log"; : > "$OCCLOG"
( cd "$S/wt" && exec perl -e 'use POSIX; setsid(); exec "sleep", "300"' ) & HOLD=$!
disown "$HOLD" 2>/dev/null || true
sleep 0.6
out=$(cd "$S" && OCC_STUB_LOG="$OCCLOG" OCC_STUB_MODE=flip FNO_WT_OCCUPANCY_CMD="$STUB/classify" bash "$LIFECYCLE" cleanup --merged --apply 2>&1); rc=$?
if [[ $rc -eq 0 && -d "$S/wt" ]] && echo "$out" | grep -q "kept (needs-confirmation)"; then
    pass "flip between reads keeps the tree as needs-confirmation (AC14)"
else
    fail "AC14 flip" "rc=$rc exists=$([[ -d "$S/wt" ]] && echo y || echo n) log=[$(cat "$OCCLOG" 2>/dev/null)] out=[$out]"
fi
if grep -q 'called' "$OCCLOG" && [[ "$(grep -c called "$OCCLOG")" -ge 2 ]]; then
    pass "both reads went through the classifier (AC14)"
else
    fail "AC14 two reads" "log=[$(cat "$OCCLOG")]"
fi
kill "$HOLD" 2>/dev/null
rm -rf "$S" "$STUB"

echo ""
echo "worktree lifecycle: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
