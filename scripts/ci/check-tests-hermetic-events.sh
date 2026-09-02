#!/usr/bin/env bash
# Fails when one of the dispatch-observability test modules names neither
# journal env var (FNO_TEST_HERMETIC / FNO_EVENTS_PATH). The audit measured
# 224 fixture rows and 212 rows naming a nonexistent node in the developer's
# live global journal - fixtures a green run had written to production. The
# conftest fixture _plan_hermetic_events_journal does the per-test pinning;
# this guard proves the marker on the test-file side did not rot. Keep the
# file list in step with _PLAN_JOURNAL_PINNED_MODULES in cli/tests/conftest.py.
set -euo pipefail

files=(
  "cli/tests/agents/test_spawn_gate_refusal_events.py"
  "cli/tests/unit/test_advance_explain.py"
  "cli/tests/agents/test_agents_top.py"
  "cli/tests/unit/test_epic_status.py"
  "cli/tests/unit/test_join_events.py"
)

fail=0
for f in "${files[@]}"; do
  if [[ ! -f "$f" ]]; then
    echo "FAIL: $f is missing (renamed? update this guard and the conftest list)" >&2
    fail=1
    continue
  fi
  if ! grep -qE "FNO_TEST_HERMETIC|FNO_EVENTS_PATH" "$f"; then
    echo "FAIL: $f names neither FNO_TEST_HERMETIC nor FNO_EVENTS_PATH - its journal isolation is undeclared" >&2
    fail=1
  fi
done
if [[ "$fail" -eq 0 ]]; then
  echo "ok: all ${#files[@]} dispatch-observability test modules declare their journal isolation"
fi
exit "$fail"
