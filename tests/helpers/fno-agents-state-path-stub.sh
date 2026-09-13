#!/usr/bin/env bash
# Test double for `fno-agents state path <name>`, the verb
# hooks/helpers/init-target-state.sh uses to resolve its manifest location.
# Every other verb delegates to the next real fno-agents on PATH (skipping this
# stub's own dir), so harnesses that exercise the real claim path get real
# claim behavior; with no real binary on PATH the call exits 1, the same as a
# CI runner with no binary.
if [[ "${1:-} ${2:-}" == "state path" && -n "${FNO_TEST_SPACE:-}" ]]; then
  case "${3:-}" in
    target-state) printf '%s\n' "$FNO_TEST_SPACE/target-state.md" ;;
    events) printf '%s\n' "$FNO_TEST_SPACE/events.jsonl" ;;
    *) printf '%s\n' "$FNO_TEST_SPACE/$3" ;;
  esac
  exit 0
fi

_self_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
_self="${BASH_SOURCE[0]:-$0}"
_oldifs="$IFS"
IFS=':'
for _d in $PATH; do
  IFS="$_oldifs"
  [[ "$_d" == "$_self_dir" ]] && continue
  [[ -x "$_d/fno-agents" ]] || continue
  # -ef catches a relative PATH entry that resolves to this same file, where
  # exec would re-enter the stub forever.
  [[ "$_d/fno-agents" -ef "$_self" ]] && continue
  exec "$_d/fno-agents" "$@"
done
IFS="$_oldifs"
exit 1
