#!/usr/bin/env bash
# Tests for scripts/prune-fno-dir.sh
# Run from any directory - uses FNO_DIR env var to redirect away
# from the real ~/.fno/. The real directory is never touched.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PRUNE="$SCRIPT_DIR/../prune-fno-dir.sh"
PASS=0
FAIL=0

if [[ ! -f "$PRUNE" ]]; then
    echo "FAIL: $PRUNE not found - cannot run tests"
    exit 1
fi

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

# fixture_dir <case_name> -> echoes a fresh temp dir seeded with a
# deletable file, an archivable file, and a keep file.
fixture_dir() {
    local name="$1"
    local d
    d=$(mktemp -d)
    mkdir -p "$d/abilities"
    : > "$d/abilities/tasks.json"          # delete target
    : > "$d/abilities/.DS_Store"           # delete target
    : > "$d/abilities/settings.yaml.bak"   # archive target
    : > "$d/abilities/graph.json"          # keep (untouched)
    echo "$d"
}

# ---- T01: --help exits 0 and mentions usage ----
echo "T01: --help"
HELP_OUT=$(bash "$PRUNE" --help 2>&1)
HELP_RC=$?
if [[ $HELP_RC -eq 0 ]]; then pass "rc=0"; else fail "rc=$HELP_RC (expected 0)"; fi
if [[ -n "$HELP_OUT" ]]; then pass "help printed something"; else fail "help output empty"; fi
if echo "$HELP_OUT" | grep -q -- "--apply"; then pass "mentions --apply"; else fail "no --apply in help"; fi
if echo "$HELP_OUT" | grep -q -i "dry-run"; then pass "mentions dry-run"; else fail "no dry-run in help"; fi

# ---- T02: dry-run leaves files alone ----
echo "T02: dry-run leaves files alone"
F=$(fixture_dir t02)
FNO_DIR="$F/abilities" bash "$PRUNE" >/dev/null 2>&1
RC=$?
if [[ $RC -eq 0 ]]; then pass "dry-run rc=0"; else fail "dry-run rc=$RC (expected 0)"; fi
if [[ -f "$F/abilities/tasks.json" ]]; then pass "tasks.json preserved"; else fail "tasks.json deleted by dry-run"; fi
if [[ -f "$F/abilities/.DS_Store" ]]; then pass ".DS_Store preserved"; else fail ".DS_Store deleted by dry-run"; fi
if [[ -f "$F/abilities/settings.yaml.bak" ]]; then pass "settings.yaml.bak preserved"; else fail "settings.yaml.bak moved by dry-run"; fi
if [[ ! -d "$F/abilities/archive" ]]; then pass "archive dir not created in dry-run"; else fail "archive dir created in dry-run"; fi
rm -rf "$F"

# ---- T03: --apply deletes + archives correctly ----
echo "T03: --apply deletes + archives"
F=$(fixture_dir t03)
FNO_DIR="$F/abilities" bash "$PRUNE" --apply >/dev/null 2>&1
RC=$?
if [[ $RC -eq 0 ]]; then pass "apply rc=0"; else fail "apply rc=$RC (expected 0)"; fi
if [[ ! -e "$F/abilities/tasks.json" ]]; then pass "tasks.json deleted"; else fail "tasks.json still present"; fi
if [[ ! -e "$F/abilities/.DS_Store" ]]; then pass ".DS_Store deleted"; else fail ".DS_Store still present"; fi
if [[ ! -e "$F/abilities/settings.yaml.bak" ]]; then pass "settings.yaml.bak moved"; else fail "settings.yaml.bak still in place"; fi
if [[ -f "$F/abilities/archive/2026-04/settings.yaml.bak" ]]; then pass "settings.yaml.bak in archive"; else fail "settings.yaml.bak missing from archive"; fi
if [[ -f "$F/abilities/graph.json" ]]; then pass "graph.json untouched"; else fail "graph.json deleted (out-of-scope!)"; fi
rm -rf "$F"

# ---- T04: idempotent --apply re-run is a no-op success ----
echo "T04: idempotent re-run"
F=$(fixture_dir t04)
FNO_DIR="$F/abilities" bash "$PRUNE" --apply >/dev/null 2>&1
RC1=$?
FNO_DIR="$F/abilities" bash "$PRUNE" --apply >/dev/null 2>&1
RC2=$?
if [[ $RC1 -eq 0 && $RC2 -eq 0 ]]; then pass "both runs rc=0"; else fail "rc1=$RC1 rc2=$RC2 (expected 0,0)"; fi
if [[ -f "$F/abilities/archive/2026-04/settings.yaml.bak" ]]; then pass "archive intact after re-run"; else fail "archive disturbed by re-run"; fi
if [[ -f "$F/abilities/graph.json" ]]; then pass "keep file intact after re-run"; else fail "keep file affected by re-run"; fi
rm -rf "$F"

# ---- T05: missing FNO_DIR exits non-zero ----
echo "T05: missing dir errors out"
FNO_DIR="/nonexistent-prune-test-dir-$$" bash "$PRUNE" --apply >/dev/null 2>&1
RC=$?
if [[ $RC -ne 0 ]]; then pass "rc=$RC (expected non-zero)"; else fail "rc=0 on missing dir"; fi

# ---- T06: investigate items always advertised, never touched ----
echo "T06: investigate items advertised but not touched"
F=$(fixture_dir t06)
mkdir -p "$F/abilities/signals"
: > "$F/abilities/convo-signals.jsonl"
OUT=$(FNO_DIR="$F/abilities" bash "$PRUNE" --apply 2>&1)
if echo "$OUT" | grep -q "convo-signals"; then pass "convo-signals listed"; else fail "convo-signals missing from advisory output"; fi
if echo "$OUT" | grep -q "INVESTIGATE"; then pass "INVESTIGATE header present"; else fail "INVESTIGATE header missing"; fi
if [[ -f "$F/abilities/convo-signals.jsonl" ]]; then pass "convo-signals.jsonl untouched"; else fail "convo-signals.jsonl was modified"; fi
if [[ -d "$F/abilities/signals" ]]; then pass "signals/ untouched"; else fail "signals/ was modified"; fi
rm -rf "$F"

# ---- T07: unknown flag exits with usage error ----
echo "T07: unknown flag rejected"
ERR=$(bash "$PRUNE" --bogus 2>&1)
RC=$?
if [[ $RC -ne 0 ]]; then pass "rc=$RC (expected non-zero)"; else fail "rc=0 on unknown flag"; fi
if echo "$ERR" | grep -q -i "unknown argument"; then pass "error message helpful"; else fail "error message missing"; fi

# ---- T08: archive collision with identical bytes -> reconciles silently ----
echo "T08: archive collision (identical) reconciles"
F=$(fixture_dir t08)
mkdir -p "$F/abilities/archive/2026-04"
# pre-seed dest with identical bytes to source
echo "same-bytes" > "$F/abilities/settings.yaml.bak"
echo "same-bytes" > "$F/abilities/archive/2026-04/settings.yaml.bak"
FNO_DIR="$F/abilities" bash "$PRUNE" --apply >/dev/null 2>&1
RC=$?
if [[ $RC -eq 0 ]]; then pass "rc=0 on identical-bytes reconciliation"; else fail "rc=$RC (expected 0)"; fi
if [[ ! -e "$F/abilities/settings.yaml.bak" ]]; then pass "source removed after reconciliation"; else fail "source still present (idempotency leak)"; fi
if [[ -f "$F/abilities/archive/2026-04/settings.yaml.bak" ]]; then pass "dest preserved"; else fail "dest removed unexpectedly"; fi
rm -rf "$F"

# ---- T09: archive collision with different bytes -> conflict, rc!=0, both preserved ----
echo "T09: archive collision (divergent) flags conflict"
F=$(fixture_dir t09)
mkdir -p "$F/abilities/archive/2026-04"
echo "new-content" > "$F/abilities/settings.yaml.bak"
echo "old-content" > "$F/abilities/archive/2026-04/settings.yaml.bak"
FNO_DIR="$F/abilities" bash "$PRUNE" --apply >"$F/out" 2>"$F/err"
RC=$?
if [[ $RC -eq 3 ]]; then pass "rc=3 on conflict"; else fail "rc=$RC (expected 3)"; fi
if [[ -f "$F/abilities/settings.yaml.bak" ]]; then pass "source preserved"; else fail "source destroyed during conflict"; fi
if [[ -f "$F/abilities/archive/2026-04/settings.yaml.bak" ]]; then pass "dest preserved"; else fail "dest destroyed during conflict"; fi
if grep -q "CONFLICT" "$F/err"; then pass "conflict surfaced on stderr"; else fail "conflict not surfaced"; fi
rm -rf "$F"

# ---- T10: project-dir scope guard ----
echo "T10: project-dir scope guard (AC3-HP / AC3-EDGE)"
# A project state dir is one inside a git work tree. Seed a repo with a .fno/
# containing tasks.json (the file the old global table would delete).
PD=$(mktemp -d)
git -C "$PD" init -q
mkdir -p "$PD/.fno"
: > "$PD/.fno/tasks.json"
: > "$PD/.fno/.DS_Store"
# AC3-HP: --apply on a project dir refuses (rc 4) and deletes nothing.
OUT=$(FNO_DIR="$PD/.fno" bash "$PRUNE" --apply 2>&1); RC=$?
if [[ $RC -eq 4 ]]; then pass "rc=4 refusing --apply on project dir"; else fail "rc=$RC (expected 4)"; fi
if [[ -f "$PD/.fno/tasks.json" ]]; then pass "tasks.json NOT deleted (guarded)"; else fail "tasks.json deleted despite guard"; fi
if echo "$OUT" | grep -q -i "REFUSING"; then pass "refusal surfaced"; else fail "refusal not surfaced"; fi
# AC3-EDGE: dry-run on a project dir is a clean no-op that warns.
OUT=$(FNO_DIR="$PD/.fno" bash "$PRUNE" 2>&1); RC=$?
if [[ $RC -eq 0 ]]; then pass "dry-run on project dir rc=0"; else fail "dry-run rc=$RC (expected 0)"; fi
if [[ -f "$PD/.fno/tasks.json" ]]; then pass "dry-run left tasks.json"; else fail "dry-run deleted tasks.json"; fi
if echo "$OUT" | grep -q -i "WARNING"; then pass "dry-run warns about project scope"; else fail "no project-scope warning"; fi
# --force overrides the guard and applies the table.
FNO_DIR="$PD/.fno" bash "$PRUNE" --apply --force >/dev/null 2>&1; RC=$?
if [[ $RC -eq 0 ]]; then pass "--force apply rc=0"; else fail "--force rc=$RC (expected 0)"; fi
if [[ ! -e "$PD/.fno/tasks.json" ]]; then pass "--force deletes tasks.json (override works)"; else fail "--force did not delete"; fi
rm -rf "$PD"

# ---- summary ----
TOTAL=$((PASS + FAIL))
echo
if [[ $FAIL -eq 0 ]]; then
    echo "PASS: prune-fno-dir.sh tests ($PASS/$TOTAL)"
    exit 0
else
    echo "FAIL: prune-fno-dir.sh tests ($PASS passed, $FAIL failed of $TOTAL)"
    exit 1
fi
