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
unset FILE_BUDGET_LINES PY_TREE_ALLOWANCE PR_BASE_REF PR_REMOTE FILE_BUDGET_BASE_SHA

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

# --- the Python tree allowance ------------------------------------------------
fresh; lines 150 grow >> cli/src/fno/keep.py; commit
check 'growth over the allowance is refused' 1 'net +150 (allowance 100)'

fresh; git rm -q cli/src/fno/dead.py; commit
check 'a deleted module banks its lines' 0 'cli/src/fno net -50, allowance 100'

fresh; git rm -q cli/src/fno/dead.py; lines 120 grow >> cli/src/fno/sub/old.py; commit
check 'a deleted module offsets nested growth' 0 'cli/src/fno net +70, allowance 100'

fresh; git mv scripts/tool.py cli/src/fno/tool.py; commit
check 'a module moved into the tree counts as growth' 1 'net +150 (allowance 100)'

fresh; mkdir -p cli/src/fno/tests; lines 150 t > cli/src/fno/tests/test_x.py; commit
check 'test files do not count against the tree' 0 'cli/src/fno net +0, allowance 100'

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
if [[ "$out" == *'net +150'* && "$out" != *'not counted'* ]]; then
  PASS=$((PASS + 1))
else
  FAIL=$((FAIL + 1))
  printf 'FAIL: a clean tree prints no uncommitted warning\n%s\n' "$out"
fi

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
