#!/usr/bin/env bash
# Runs every cli/tests/smoke/test_*.sh, collects failures, summarizes.
set -uo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
SMOKE_DIR="$REPO_ROOT/cli/tests/smoke"

cd "$REPO_ROOT"

if [[ ! -d "$SMOKE_DIR" ]]; then
  echo "FAIL: smoke dir missing: $SMOKE_DIR" >&2
  exit 1
fi

declare -a TESTS
while IFS= read -r -d '' f; do
  TESTS+=("$f")
done < <(find "$SMOKE_DIR" -maxdepth 1 -type f -name 'test_*.sh' -print0 | sort -z)

total=${#TESTS[@]}
if [[ $total -eq 0 ]]; then
  echo "FAIL: no smoke tests found in $SMOKE_DIR" >&2
  exit 1
fi

passed=0
failed=0
declare -a FAILURES

for t in "${TESTS[@]}"; do
  name=$(basename "$t")
  printf "== %s ==\n" "$name"
  if bash "$t"; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    FAILURES+=("$name")
  fi
  printf "\n"
done

printf "Summary: %d/%d tests passed\n" "$passed" "$total"
if [[ $failed -gt 0 ]]; then
  printf "Failed tests:\n"
  for f in "${FAILURES[@]}"; do
    printf "  - %s\n" "$f"
  done
  exit 1
fi

printf "%d tests passed\n" "$passed"
exit 0
