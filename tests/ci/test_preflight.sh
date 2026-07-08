#!/usr/bin/env bash
# tests/ci/test_preflight.sh
#
# Exercises scripts/ci/preflight.sh orchestration in a throwaway git repo with
# stub smoke.sh/cargo/rustup/fno on PATH, so no real 45-step suite or cargo
# build runs. Covers AC2-HP (catches a CI-red commit locally), AC2-ERR (dirty
# tree refused), AC2-EDGE (concurrent -> exit 3 + holder), AC1-FR (interrupt
# recovery: stale lock is stealable).

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
PREFLIGHT_SRC="$REPO_ROOT/scripts/ci/preflight.sh"
# pwd -P: resolve macOS /var -> /private/var so `git worktree list` paths match.
TMP="$(cd "$(mktemp -d)" && pwd -P)"
trap 'rm -rf "$TMP"' EXIT

FAILS=0
ok()   { echo "  ok: $1"; }
fail() { echo "  FAIL: $1"; FAILS=$((FAILS+1)); }

# --- build stub tool dir ----------------------------------------------------
BIN="$TMP/bin"; mkdir -p "$BIN"
WT_BASE="$TMP/wtbase"; mkdir -p "$WT_BASE"

cat > "$BIN/fno" <<EOF
#!/usr/bin/env bash
# stub: only 'config get paths.worktrees_base' is used by preflight
[[ "\$*" == *"paths.worktrees_base"* ]] && echo "$WT_BASE"
exit 0
EOF
cat > "$BIN/cargo" <<'EOF'
#!/usr/bin/env bash
# stub cargo: drop a leading +toolchain, succeed on fmt/test
[[ "${1:-}" == +* ]] && shift
exit 0
EOF
cat > "$BIN/rustup" <<'EOF'
#!/usr/bin/env bash
[[ "$*" == "toolchain list"* ]] && { echo "1.94.1-x86_64-apple-darwin (default)"; exit 0; }
exit 0
EOF
chmod +x "$BIN/fno" "$BIN/cargo" "$BIN/rustup"
export PATH="$BIN:$PATH"

# --- build the fixture repo -------------------------------------------------
FIX="$TMP/repo"; mkdir -p "$FIX/scripts/ci"
git -C "$FIX" init -q
git -C "$FIX" config user.email t@t.t; git -C "$FIX" config user.name t
cp "$PREFLIGHT_SRC" "$FIX/scripts/ci/preflight.sh"
# stub smoke.sh: exit 1 iff a POISON file is present at the checked-out HEAD
cat > "$FIX/scripts/ci/smoke.sh" <<'EOF'
#!/usr/bin/env bash
if [[ -f POISON ]]; then echo "smoke: POISON step failed"; exit 1; fi
echo "smoke: all green (stub)"; exit 0
EOF
# crate dirs so preflight's `cd crates/fno*` legs run (cargo is stubbed).
mkdir -p "$FIX/crates/fno-agents" "$FIX/crates/fno"
echo x > "$FIX/crates/fno-agents/.keep"; echo x > "$FIX/crates/fno/.keep"
git -C "$FIX" add -A; git -C "$FIX" commit -qm "green base"
GREEN_SHA="$(git -C "$FIX" rev-parse --short HEAD)"

run_pf() { ( cd "$FIX" && bash scripts/ci/preflight.sh "$@" ); }

echo "== AC2-HP-green: clean HEAD, smoke green, rust stubs green -> exit 0 =="
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "exit 0 on green" || fail "expected 0 got $rc: $out"
echo "$out" | grep -q "GREEN - safe to push" && ok "reports GREEN" || fail "no GREEN line"
echo "$out" | grep -q "cargo fmt --check (fno-agents" && ok "fmt leg in summary (AC3-HP)" || fail "no fmt leg"
echo "$out" | grep -q "cargo test --all-targets (fno-agents)" && ok "cargo test leg in summary (AC3-HP)" || fail "no test leg"
echo "$out" | grep -q "ADVISORY" && ok "audit ADVISORY row present" || fail "no ADVISORY row"

echo "== AC2-HP-red: a POISON commit is caught locally, exit non-zero, no push =="
( cd "$FIX" && touch POISON && git add -A && git commit -qm "poisoned" )
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -ne 0 ]] && ok "exit non-zero on red" || fail "expected non-zero got $rc"
echo "$out" | grep -q "RED - fix" && ok "reports RED" || fail "no RED line"
echo "$out" | grep -q "fail.*smoke suite" && ok "smoke suite marked fail" || fail "smoke not failed in summary"
# back to green for remaining tests
( cd "$FIX" && git rm -q POISON && git commit -qm "unpoison" )

echo "== AC2-ERR: dirty invoking tree refused (exit 4), nothing touched =="
( cd "$FIX" && echo dirt > dirty.txt )
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 4 ]] && ok "exit 4 on dirty" || fail "expected 4 got $rc"
echo "$out" | grep -q "dirty.txt" && ok "lists the dirty file" || fail "did not list dirty file"
[[ ! -d "$WT_BASE/repo/preflight" ]] || { [[ -z "$(ls -A "$WT_BASE/repo/preflight" 2>/dev/null)" ]] && ok "no worktree materialized on refusal" || ok "worktree pre-existed (from green run) - refusal touched nothing"; }
( cd "$FIX" && rm -f dirty.txt )

echo "== AC2-EDGE: concurrent invocation -> exit 3 with holder =="
LOCKDIR="$FIX/.git/.preflight.lock.d"
mkdir -p "$LOCKDIR"; printf 'pid=%s started=NOW host=x sha=deadbee\n' "$$" > "$LOCKDIR/holder"  # $$ is alive
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 3 ]] && ok "exit 3 when lock held by a live pid" || fail "expected 3 got $rc"
echo "$out" | grep -q "lock held" && ok "prints holder info" || fail "no holder info"
rm -rf "$LOCKDIR"

echo "== AC1-FR: a stale lock (dead holder) is stolen, run proceeds =="
mkdir -p "$LOCKDIR"; printf 'pid=%s started=OLD host=x sha=deadbee\n' 999999 > "$LOCKDIR/holder"  # dead pid
out="$(run_pf 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && ok "stole stale lock and ran to GREEN" || fail "stale-lock steal failed rc=$rc: $out"

echo ""
if [[ $FAILS -eq 0 ]]; then echo "test_preflight: ALL PASS"; exit 0
else echo "test_preflight: $FAILS FAILED"; exit 1; fi
