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
        git -c user.email=t@t -c user.name=t commit --allow-empty -qm init
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

echo ""
echo "worktree lifecycle: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
