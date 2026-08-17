#!/usr/bin/env bash
# test-postinstall-verify-wait.sh
#
# `uv tool install` exits before its own artifacts settle. The console script
# `<tools>/fno/bin/fno-py` is deleted and recreated across an install and is
# absent for ~490ms, a gap that closed only ~40ms before uv exited in an idle
# measurement (docs/architecture/cli-lazy-imports.md). A verify firing the
# instant uv returns therefore races the install it is verifying, and the
# postinstall path refused a tree that was about to be fine.
#
# This is the check that would have caught it: a verify whose subject appears
# late must succeed, and one whose subject never appears must still fail
# bounded. The second half is what keeps this a re-check rather than the blind
# sleep-retry the lazy-imports doc rejects.
#
# Exit codes: 0 pass / 1 assertion failed / 77 skipped (missing deps)

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/../.claude-plugin/postinstall.sh"

[[ -r "$SCRIPT" ]] || { echo "skip: no postinstall.sh at $SCRIPT"; exit 77; }

fail() { echo "FAIL: $*" >&2; exit 1; }

# Pull just the wait out of the script. Sourcing the whole file would run its
# installer, so this extracts the two functions under test by name.
helper="$(mktemp)"
trap 'rm -f "$helper"' EXIT
sed -n '/^uv_install_verifies_within() {/,/^}/p' "$SCRIPT" > "$helper"
[[ -s "$helper" ]] || fail "could not extract uv_install_verifies_within from $SCRIPT"

# 1. A subject that lands late must be waited for, not refused.
#    The stub fails twice, then succeeds, standing in for a console script that
#    appears a few hundred ms after uv exits.
tries_file="$(mktemp)"; echo 0 > "$tries_file"
# shellcheck disable=SC1090
source "$helper"
uv_install_verifies() {
  local n; n=$(<"$tries_file"); n=$((n + 1)); echo "$n" > "$tries_file"
  [[ "$n" -ge 3 ]]
}
start=$SECONDS
if ! uv_install_verifies_within; then
  fail "a subject that appears on the 3rd check must verify, not refuse"
fi
[[ "$(<"$tries_file")" -eq 3 ]] || fail "expected exactly 3 checks, got $(<"$tries_file")"

# 2. A subject that never appears must still fail, and stay bounded.
echo 0 > "$tries_file"
uv_install_verifies() {
  local n; n=$(<"$tries_file"); n=$((n + 1)); echo "$n" > "$tries_file"
  return 1
}
if uv_install_verifies_within; then
  fail "a broken install must still fail: nothing may be masked"
fi
checks="$(<"$tries_file")"
[[ "$checks" -eq 16 ]] || fail "expected 16 checks (1 + 15 retries), got $checks"
elapsed=$((SECONDS - start))
[[ "$elapsed" -le 10 ]] || fail "wait is not bounded: took ${elapsed}s"

rm -f "$tries_file"
echo "PASS: postinstall verify waits for a late artifact and still fails bounded on a broken one"
exit 0
