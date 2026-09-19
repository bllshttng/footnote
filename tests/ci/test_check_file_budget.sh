#!/usr/bin/env bash
# tests/ci/test_check_file_budget.sh
#
# Exercises scripts/ci/check-file-budget.sh against scratch repos: a local
# origin holding main, and a feature branch whose diff is the case under test.
# Every case asserts the exit code AND a line only that outcome prints, so a
# gate that skipped a path cannot pass by printing nothing.
#
# Run: bash tests/ci/test_check_file_budget.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$(cd "${SCRIPT_DIR}/../.." && pwd)/scripts/ci/check-file-budget.sh"
[[ -f "$GATE" ]] || { echo "gate not found at $GATE" >&2; exit 1; }

# The caller's git config (signing, hooks) must not reach the scratch repos.
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
unset FILE_BUDGET_LINES PY_ADDED_BUDGET PR_BASE_REF PR_REMOTE FILE_BUDGET_BASE_SHA FILE_BUDGET_EXCEPTION_LABEL

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

PASS=0; FAIL=0

# Distinct content per file, so rename detection pairs only what a case moves.
lines() { seq -f "$2 %g" 1 "$1"; }

# fresh: a new origin whose main holds the base files, and a feature branch
# checked out on top of it. The caller stays in the work tree.
fresh() {
  rm -rf "$TMP/origin.git" "$TMP/repo"
  git init -q --bare "$TMP/origin.git"
  git init -q "$TMP/repo"
  cd "$TMP/repo" || exit 1
  git symbolic-ref HEAD refs/heads/main
  mkdir -p cli/src/fno/sub scripts
  lines 50 dead > cli/src/fno/dead.py
  lines 10 keep > cli/src/fno/keep.py
  lines 30 old > cli/src/fno/sub/old.py
  lines 150 tool > scripts/tool.py
  lines 40 big > scripts/big.sh
  git add -A && git commit -qm base
  git remote add origin "$TMP/origin.git"
  git push -q origin main
  git checkout -q -b feature
}

commit() { git add -A && git commit -qm change; }

# check <label> <want_exit> <marker> [VAR=value...]
check() {
  local label="$1" want="$2" marker="$3" out got
  shift 3
  out="$(env "$@" bash "$GATE" 2>&1)"; got=$?
  if [[ "$got" -eq "$want" && "$out" == *"$marker"* ]]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s\n  want exit %s with: %s\n  got exit %s:\n%s\n' "$label" "$want" "$marker" "$got" "$out"
  fi
}

# --- the Python added-line budget ---------------------------------------------
fresh; lines 150 grow >> cli/src/fno/keep.py; commit
check 'growth over the added-line budget is refused' 1 'added +150 lines (added-line budget 30'

fresh; git rm -q cli/src/fno/dead.py; commit
check 'a deleted module adds nothing' 0 'cli/src/fno added +0, budget 30'

fresh; git rm -q cli/src/fno/dead.py; lines 120 grow >> cli/src/fno/sub/old.py; commit
check 'a deleted module does not offset nested growth' 1 'added +120 lines (added-line budget 30'

fresh; git mv scripts/tool.py cli/src/fno/tool.py; commit
check 'a module moved into the tree counts as growth' 1 'added +150 lines (added-line budget 30'

fresh; mkdir -p cli/src/fno/tests; lines 150 t > cli/src/fno/tests/test_x.py; commit
check 'test files do not count against the tree' 0 'cli/src/fno added +0, budget 30'

# The specimen that proved the hole: a branch that deletes far more than it
# adds is still refused, because the ceiling is on added lines, never net.
fresh; lines 152 extra > cli/src/fno/extra.py; commit
git rm -q cli/src/fno/extra.py cli/src/fno/dead.py cli/src/fno/sub/old.py
lines 101 fresh > cli/src/fno/fresh.py; commit
check 'the delete-heavy rewrite is refused on added lines' 1 'added +101 lines (added-line budget 30'

fresh; lines 12 grow >> cli/src/fno/keep.py; commit
check 'added lines under the budget pass' 0 'cli/src/fno added +12, budget 30'

fresh; lines 12 grow >> cli/src/fno/keep.py; commit
check 'the env override moves the budget' 1 'budget 5' PY_ADDED_BUDGET=5

# A config answer moves the budget when no env override is set.
CONFIGBIN="$(mktemp -d)"
cat > "$CONFIGBIN/fno" <<'STUB'
#!/bin/bash
if [[ "${1:-} ${2:-}" == "config get" && "${3:-}" == "blueprint.python_repair_added_lines" ]]; then
    echo 5
    exit 0
fi
exit 1
STUB
chmod +x "$CONFIGBIN/fno"
fresh; lines 12 grow >> cli/src/fno/keep.py; commit
check 'the config key moves the budget' 1 'budget 5' PATH="$CONFIGBIN:$PATH"
rm -rf "$CONFIGBIN"

# --- the operator label exception ---------------------------------------------
fresh; lines 150 grow >> cli/src/fno/keep.py; commit
check 'the label waives the tree allowance and names itself' 0 \
  'label file-budget-exception waives the tree allowance' \
  FILE_BUDGET_EXCEPTION_LABEL=file-budget-exception

fresh; lines 20 grow >> cli/src/fno/keep.py; commit
out="$(FILE_BUDGET_EXCEPTION_LABEL=file-budget-exception bash "$GATE" 2>&1)"
if [[ "$out" == *'added +20, budget 30'* && "$out" != *'waives'* ]]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1))
  printf 'FAIL: a within-allowance tree must not claim the waiver\n%s\n' "$out"
fi

fresh; lines 5 grow >> scripts/big.sh; commit
check 'the label never waives the per-file budget' 1 \
  'grows it by +5/-0' FILE_BUDGET_LINES=20 FILE_BUDGET_EXCEPTION_LABEL=file-budget-exception

# --- the per-file budget ------------------------------------------------------
fresh; git mv scripts/big.sh scripts/huge.sh; lines 5 grow >> scripts/huge.sh; commit
check 'a renamed over-budget file may not grow' 1 \
  'scripts/huge.sh is 45 lines (budget 20) and this change grows it by +5/-0' FILE_BUDGET_LINES=20

fresh; mkdir -p cli; git mv scripts/big.sh cli/big.sh; commit
check 'a moved over-budget file is measured against its old path' 0 \
  'ok cli/big.sh 40 lines, change +0/-0 (net 0); no grow' FILE_BUDGET_LINES=20

fresh; lines 5 grow >> scripts/big.sh; commit
check 'the push alarm names the owed shrink' 1 \
  'The next change touching this file must shrink it by at least 5 lines.' \
  FILE_BUDGET_LINES=20 FILE_BUDGET_BASE_SHA="$(git rev-parse main)"

fresh; git rm -q scripts/big.sh; commit
check 'a deleted over-budget file passes' 0 'check-file-budget: ok (no over-budget file grew' FILE_BUDGET_LINES=20

# --- uncommitted work ---------------------------------------------------------
# The diffs read commits, so a worker measuring mid-change would read +0 for
# work it has not committed. The gate must name that, never print a clean zero.
fresh; lines 150 grow >> cli/src/fno/keep.py
check 'an uncommitted edit is named, not read as no growth' 0 \
  'uncommitted changes to gated files are not counted'

fresh; lines 5 new > cli/src/fno/new.py
check 'an untracked module is named, not read as no growth' 0 \
  'uncommitted changes to gated files are not counted'

fresh; lines 150 grow >> cli/src/fno/keep.py; commit
out="$(bash "$GATE" 2>&1)"
if [[ "$out" == *'added +150'* && "$out" != *'not counted'* ]]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1))
  printf 'FAIL: a clean tree prints no uncommitted warning\n%s\n' "$out"
fi

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
