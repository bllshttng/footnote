#!/usr/bin/env bash
# tests/ci/test_check_file_budget_agents_quote.sh
#
# Exercises the tree-allowance restatement tripwire in
# scripts/ci/check-file-budget.sh: AGENTS.md restates the cli/src/fno allowance
# on the SessionStart surface, so the owner refuses (exit 2) when that
# restatement drifts. Uses a scratch git repo so the gate's own base resolution
# is real (FILE_BUDGET_BASE_SHA points at a local commit; no fetch happens).
#
# Run: bash tests/ci/test_check_file_budget_agents_quote.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT_REAL="$(cd "${SCRIPT_DIR}/../.." && pwd)"
GATE="${REPO_ROOT_REAL}/scripts/ci/check-file-budget.sh"

log()  { printf '[agents-quote] %s\n' "$*"; }
fail() { printf '[agents-quote] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[agents-quote] PASS: %s\n' "$*"; }

[[ -f "$GATE" ]] || fail "gate not found at $GATE"

WORK=$(mktemp -d -t agents-quote-XXXXXX)
trap 'rm -rf "$WORK"' EXIT

REPO="$WORK/repo"
mkdir -p "$REPO"
cd "$REPO"
git init -q
git config user.email t@t.t
git config user.name t
export GIT_AUTHOR_DATE="2026-01-01T00:00:00" GIT_COMMITTER_DATE="2026-01-01T00:00:00"

BULLET_NEW='- **File budget:** a source file over 5,000 lines, and `cli/src/fno` Python past net +100 per change, may only shrink; a feature lands in `crates/`. `scripts/ci/check-file-budget.sh` names the remedy.'
BULLET_OLD='- **File budget:** a source file over 5,000 lines is shrink-only. The refusal in `scripts/ci/check-file-budget.sh` names the remedy.'

# Base commit carries the restating bullet; the diff base..head stays empty so
# the clean path proves the assertion adds no findings of its own.
printf '%s\n' "$BULLET_NEW" > AGENTS.md
git add AGENTS.md
git commit -q -m "base: AGENTS.md restates the allowance"
BASE=$(git rev-parse HEAD)

run_gate() { FILE_BUDGET_BASE_SHA="$BASE" bash "$GATE" 2>&1; }

# --- HP: restating AGENTS.md passes with the ok line --------------------------
log "HP: restated allowance passes"
OUT=$(run_gate); RC=$?
(( RC == 0 )) || fail "HP: expected exit 0, got $RC ($OUT)"
echo "$OUT" | grep -q "check-file-budget: ok" || fail "HP: missing ok line ($OUT)"
echo "$OUT" | grep -q "allowance 100" || fail "HP: ok line does not name the allowance ($OUT)"
pass "HP: restated AGENTS.md exits 0"

# --- ERR: drifted AGENTS.md exits 2, naming the allowance --------------------
log "ERR: drifted restatement exits 2"
printf '%s\n' "$BULLET_OLD" > AGENTS.md
OUT=$(run_gate); RC=$?
(( RC == 2 )) || fail "ERR: expected exit 2, got $RC ($OUT)"
echo "$OUT" | grep -q "no longer quotes the tree allowance (net +100)" \
    || fail "ERR: refusal does not name the allowance ($OUT)"
echo "$OUT" | grep -q "file-budget bullet" || fail "ERR: refusal does not name the bullet ($OUT)"
pass "ERR: drifted AGENTS.md exits 2 with allowance named"

# --- SKIP: an env override is caller configuration, never the stated rule ----
log "SKIP: env override bypasses the assertion"
OUT=$(FILE_BUDGET_BASE_SHA="$BASE" PY_TREE_ALLOWANCE=100 bash "$GATE" 2>&1); RC=$?
(( RC == 0 )) || fail "SKIP: expected exit 0 under override, got $RC ($OUT)"
pass "SKIP: PY_TREE_ALLOWANCE set bypasses the assertion"

# --- SKIP: repos that ship the script without the bullet are not refused -----
log "SKIP: missing AGENTS.md is not a refusal"
rm AGENTS.md
OUT=$(run_gate); RC=$?
(( RC == 0 )) || fail "SKIP: expected exit 0 without AGENTS.md, got $RC ($OUT)"
pass "SKIP: consumer repos without AGENTS.md pass"

echo "[agents-quote] all check-file-budget agents-quote tests passed"
exit 0
