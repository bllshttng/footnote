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
declare -a LEAKS

# Entry count of the developer's REAL graph, read-only. A smoke test that
# forgets to redirect HOME writes here, and the suite stays green: the pytest
# tree solved this class once (conftest.py's module-load HOME redirect); the
# shell tree had no counterpart. A missing graph counts as 0 so the first leak
# onto a fresh machine still trips; any other read failure prints nothing,
# disabling the check rather than reddening the suite on unrelated breakage.
graph_entry_count() {
  python3 -c "
import json, os, sys
path = os.path.expanduser('~/.fno/graph.json')
if not os.path.exists(path):
    print(0)
    sys.exit(0)
try:
    with open(path) as fh:
        print(len(json.load(fh)['entries']))
except Exception:
    sys.exit(0)
" 2>/dev/null
}

for t in "${TESTS[@]}"; do
  name=$(basename "$t")
  printf "== %s ==\n" "$name"
  graph_before=$(graph_entry_count)
  if bash "$t"; then
    passed=$((passed + 1))
  else
    failed=$((failed + 1))
    FAILURES+=("$name")
  fi
  graph_after=$(graph_entry_count)
  if [[ -n "$graph_before" && -n "$graph_after" && "$graph_after" -gt "$graph_before" ]]; then
    LEAKS+=("$name ($graph_before -> $graph_after)")
  fi
  printf "\n"
done

printf "Summary: %d/%d tests passed\n" "$passed" "$total"
if [[ ${#LEAKS[@]} -gt 0 ]]; then
  printf "Leaked into the real ~/.fno/graph.json:\n" >&2
  for l in "${LEAKS[@]}"; do
    printf "  - %s\n" "$l" >&2
  done
  printf "Redirect HOME to the test's scratch dir so fno writes land there.\n" >&2
fi
if [[ $failed -gt 0 ]]; then
  printf "Failed tests:\n"
  for f in "${FAILURES[@]}"; do
    printf "  - %s\n" "$f"
  done
  exit 1
fi
if [[ ${#LEAKS[@]} -gt 0 ]]; then
  exit 1
fi

printf "%d tests passed\n" "$passed"
exit 0
