#!/usr/bin/env bash
# test_uv_install_verify_wait.sh
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
REPO="$(cd "$HERE/../.." && pwd)"
SCRIPT="$REPO/.claude-plugin/postinstall.sh"

[[ -r "$SCRIPT" ]] || { echo "skip: no postinstall.sh at $SCRIPT"; exit 77; }

fail() { echo "FAIL: $*" >&2; exit 1; }

# Pull just the wait out of the script. Sourcing the whole file would run its
# installer, so this extracts the one function under test by name; the predicate
# it re-checks is stubbed below.
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

# 3. The SAME bounded re-check, on every provisioning path, with the same 3s.
#    A guard on one of N paths is decorative, and this one has been proven so
#    twice: `scripts/install/fno.sh` and update.py's install verify both shipped
#    single-shot while the other paths waited, and each refused a good install
#    on its own. Nothing else can pin numbers across Rust, bash, and emitted
#    POSIX sh, so this is where drift fails.
have() { # have <file> <fixed-string> <what it must keep>
  local f="$REPO/$1"
  [[ -r "$f" ]] || fail "cannot read $1 (the pin is only as good as the file)"
  grep -qF -- "$2" "$f" || fail "$1 lost its $3 (looked for: $2)"
}
lacks() { # lacks <file> <fixed-string> <why>
  local f="$REPO/$1"
  [[ -r "$f" ]] || fail "cannot read $1"
  grep -qF -- "$2" "$f" && fail "$1 still has a single-shot verify: $3"
  return 0
}

have .claude-plugin/postinstall.sh '-gt 15 ]] && return 1' "15-retry ceiling"
have .claude-plugin/postinstall.sh 'sleep 0.2' "0.2s poll"
have .claude-plugin/postinstall.sh 'uv_install_verifies_within ||' "waited verify call"
lacks .claude-plugin/postinstall.sh 'uv_install_verifies ||' "call the _within wrapper"

have scripts/install/fno.sh '-gt 15 ] && return 1' "15-retry ceiling"
have scripts/install/fno.sh 'sleep 0.2' "0.2s poll"
have scripts/install/fno.sh 'uv_install_verifies_within ||' "waited verify call"
lacks scripts/install/fno.sh 'uv_install_verifies ||' "call the _within wrapper"
have scripts/install/fno.sh 'print("name=" + (md.get("Name") or ""))' "key=value identity probe"
lacks scripts/install/fno.sh 'print(md["Name"])' "None-on-absent-header probe"
have scripts/install/fno.sh 'verify_ours && return 0' "waited identity verify"
lacks scripts/install/fno.sh '] && verify_ours; then' "single-shot adopt verify"
lacks scripts/install/fno.sh 'verify_ours || die' "single-shot post-install verify"

have cli/src/fno/update.py '"$__vn" -gt 15 ] && return 1; sleep 0.2' "15-retry ceiling and 0.2s poll"
have cli/src/fno/update.py 'if __fno_verify_within; then break' "waited install verify"
have cli/src/fno/update.py '_fno_n -lt 15' "15-retry ceiling in _await_binary"
# Anchored to _await_binary's own increment: a bare `sleep 0.2` here would be
# satisfied by whichever of the two polls did NOT drift, so neither would be pinned.
have cli/src/fno/update.py '_fno_n=$((_fno_n+1)); sleep 0.2' "0.2s poll in _await_binary"

have crates/fno/src/bootstrap.rs 'VERIFY_ATTEMPTS: u32 = 15' "15-retry ceiling"
have crates/fno/src/bootstrap.rs 'VERIFY_POLL: Duration = Duration::from_millis(200)' "0.2s poll"
have crates/fno/src/bootstrap.rs 'install_verified_within(uv, VERIFY_ATTEMPTS, VERIFY_POLL)' "waited verify call"

# The git-protection merge veto's timeout is sized off the doc's
# per-invocation ceiling ("Size any harness budget that shells `fno` against
# 21s"). When the wait budgets move, that number must be re-derived, and this
# pin fails until the hook follows it. Without it the four 15-pins still catch
# a budget change while this fifth consumer silently under-covers.
_hook_veto=$(grep -oE 'timeout=25' "$REPO/hooks/git-protection.py" | head -n 1)
_doc_ceiling=$(grep -oE 'against [0-9]+s' "$REPO/docs/architecture/cli-lazy-imports.md" | head -n 1)
[ -n "$_hook_veto" ] || fail "git-protection.py lost its 25s veto timeout"
[ -n "$_doc_ceiling" ] || fail "cli-lazy-imports.md lost its per-invocation sizing line"
_h=${_hook_veto//[^0-9]/}; _c=${_doc_ceiling//[^0-9]/}
[ "$_h" -ge "$_c" ] || fail "the veto timeout (${_h}s) sits under the doc's ${_c}s ceiling"

echo "PASS: postinstall verify waits for a late artifact, still fails bounded on a broken one, and all four provisioning paths share the 3s budget"
exit 0
