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

echo "== 2. _wt_pids keys on cwd, not open files =="

# Source just the helper (the script body runs a case statement on source).
eval "$(sed -n '/^_wt_pids()/,/^}/p' "$LIFECYCLE")"

if command -v lsof >/dev/null 2>&1; then
    WT=$(mktemp -d -t wt-pids.XXXXXX); mkdir -p "$WT/sub"; echo x > "$WT/sub/f"
    # (a) process cwd'd ELSEWHERE holding an open fd under WT -> must NOT match.
    ( cd /tmp && exec 9<"$WT/sub/f"; sleep 5 ) & OFF=$!
    # (b) process cwd'd INSIDE WT -> must match.
    ( cd "$WT/sub" && exec sleep 5 ) & IN=$!
    disown "$OFF" "$IN" 2>/dev/null || true   # silence job-control "Terminated" notices
    sleep 0.6
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
( cd "$WT" && exec sleep 8 ) & HOLD=$!
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
eval "$(sed -n '/^_wt_pids()/,/^}/p' "$LIFECYCLE")"
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
FOUND=$(PATH="$STUBDIR:$PATH" _wt_pids "$WT")
N_FOUND=$(printf '%s\n' "$FOUND" | grep -c .)
N_PS=$(wc -l < "$COUNTFILE" | tr -d ' ')
if [[ "$N_FOUND" -eq 5 ]]; then pass "all 5 argv-matched pids detected"; else fail "detect matches" "found $N_FOUND of 5: [$FOUND]"; fi
if [[ "$N_PS" -eq 1 ]]; then pass "exactly 1 ps call for 5 matches (was 1-per-match)"; else fail "O(1) ps calls" "ps invoked $N_PS times, want 1"; fi
rm -rf "$STUBDIR"

# 5d. Two sweeps racing to reclaim the same dead-holder lock: exactly one
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

echo ""
echo "worktree lifecycle: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
