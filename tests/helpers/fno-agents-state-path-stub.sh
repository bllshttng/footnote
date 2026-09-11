#!/usr/bin/env bash
# Test double for `fno-agents state path <name>`, the only fno-agents verb
# hooks/helpers/init-target-state.sh calls. Every other call exits 1, the same
# as a CI runner with no binary.
[[ "${1:-} ${2:-}" == "state path" && -n "${FNO_TEST_SPACE:-}" ]] || exit 1
case "${3:-}" in
  target-state) printf '%s\n' "$FNO_TEST_SPACE/target-state.md" ;;
  events) printf '%s\n' "$FNO_TEST_SPACE/events.jsonl" ;;
  *) printf '%s\n' "$FNO_TEST_SPACE/$3" ;;
esac
