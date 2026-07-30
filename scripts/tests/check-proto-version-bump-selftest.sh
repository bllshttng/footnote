#!/usr/bin/env bash
# Self-test for scripts/ci/check-proto-version-bump.sh
#
# Builds throwaway git repos rather than stubbing git, because every case worth
# testing here IS a git topology: the collision case only exists because two
# identical one-line changes merge without a conflict, and no amount of
# function-level string testing reproduces that.
#
# Cases:
#   1. clean bump (base v10, PR v11)              -> exit 0
#   2. identical parallel bump (base v11, PR v11) -> exit 1, "re-bump to v12"
#   3. const line untouched by the PR             -> exit 0
#   4. lowered version                            -> exit 1
#   5. unparseable const at the PR head           -> exit 1 (never a silent pass)
#   6. unreachable base ref                       -> exit 2 (fail closed)
#
# Usage: bash scripts/tests/check-proto-version-bump-selftest.sh
# Exit 0 = all assertions passed.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
GUARD="$REPO_ROOT/scripts/ci/check-proto-version-bump.sh"
PROTO_REL="crates/fno/src/proto.rs"

if [[ ! -f "$GUARD" ]]; then
    echo "ERROR: guard script not found: $GUARD" >&2
    exit 1
fi

TMP_BASE="$(mktemp -d)"
trap 'rm -rf "$TMP_BASE"' EXIT

failures=0
pass() { echo "  ok: $1"; }
fail() { echo "  FAIL: $1" >&2; failures=$((failures + 1)); }

write_proto() {
    # $1 = repo dir, $2 = literal const body (so case 5 can write a bad one)
    mkdir -p "$1/$(dirname "$PROTO_REL")"
    cat > "$1/$PROTO_REL" <<EOF
// fixture
$2

/// unrelated doc comment
pub const SOMETHING_ELSE: u32 = 7;
EOF
}

version_line() { printf 'pub const PROTO_VERSION: u32 = %s;' "$1"; }

# Build a repo with a "main" at $1, a feature branch off an earlier commit whose
# tip sets the version to $2, then (if $3 is given) advance main to $3 to
# simulate a parallel branch having merged first.
# Echoes "<repo_dir> <head_sha>".
make_repo() {
    local base_v="$1" head_v="$2" main_advance="${3:-}"
    local dir
    dir="$(mktemp -d "$TMP_BASE/repo.XXXXXX")"
    git -C "$dir" init --quiet -b main
    git -C "$dir" config user.email t@example.com
    git -C "$dir" config user.name test
    write_proto "$dir" "$(version_line "$base_v")"
    git -C "$dir" add -A
    git -C "$dir" commit --quiet -m "base v$base_v"

    git -C "$dir" checkout --quiet -b feature
    write_proto "$dir" "$head_v"
    git -C "$dir" add -A
    git -C "$dir" commit --quiet -m "feature"
    local head_sha
    head_sha="$(git -C "$dir" rev-parse HEAD)"

    git -C "$dir" checkout --quiet main
    if [[ -n "$main_advance" ]]; then
        write_proto "$dir" "$(version_line "$main_advance")"
        git -C "$dir" add -A
        git -C "$dir" commit --quiet -m "parallel branch merged: v$main_advance"
    fi
    # A local remote named origin, so the guard resolves origin/main exactly as
    # it does in CI (no network, no special-casing inside the guard).
    git -C "$dir" remote add origin "$dir"
    git -C "$dir" fetch --quiet origin main
    git -C "$dir" update-ref refs/remotes/origin/main "$(git -C "$dir" rev-parse main)"
    printf '%s %s\n' "$dir" "$head_sha"
}

run_guard() {
    # $1 = repo dir, $2 = head sha; echoes output, returns guard's exit code
    ( cd "$1" && PR_HEAD_SHA="$2" PR_BASE_REF=main PR_REMOTE=origin \
        bash "$GUARD" 2>&1 )
}

# --- case 1: clean bump ----------------------------------------------------
read -r dir sha <<< "$(make_repo 10 "$(version_line 11)")"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 0 ]] && [[ "$out" == *"v10 -> v11"* ]]; then
    pass "clean bump exits 0"
else
    fail "clean bump: rc=$rc out=$out"
fi

# --- case 2: the collision this guard exists for --------------------------
# Feature branch bumps 10 -> 11; main independently reached 11 and merged first.
read -r dir sha <<< "$(make_repo 10 "$(version_line 11)" 11)"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 1 ]] && [[ "$out" == *"re-bump to v12"* ]]; then
    pass "identical parallel bump exits 1 and names the re-bump target"
else
    fail "collision case: rc=$rc out=$out"
fi

# --- case 2b: the merge really is conflict-free ---------------------------
# Proves the premise rather than assuming it: if git conflicted here, the guard
# would be redundant and this whole script would be testing a non-problem.
read -r dir sha <<< "$(make_repo 10 "$(version_line 11)" 11)"
if git -C "$dir" merge --no-commit --no-ff "$sha" >/dev/null 2>&1; then
    pass "identical bumps merge without conflict (the premise holds)"
else
    fail "identical bumps conflicted - premise of this guard is wrong"
fi
git -C "$dir" merge --abort 2>/dev/null || true

# --- case 3: const line untouched ----------------------------------------
# The feature commit edits the file but not the const line.
dir="$(mktemp -d "$TMP_BASE/repo.XXXXXX")"
git -C "$dir" init --quiet -b main
git -C "$dir" config user.email t@example.com
git -C "$dir" config user.name test
write_proto "$dir" "$(version_line 10)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m base
git -C "$dir" checkout --quiet -b feature
printf '\n// an unrelated comment\n' >> "$dir/$PROTO_REL"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "comment only"
sha="$(git -C "$dir" rev-parse HEAD)"
git -C "$dir" checkout --quiet main
# main advances past the feature branch's version, which must still not matter.
write_proto "$dir" "$(version_line 12)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "main advances"
git -C "$dir" remote add origin "$dir"
git -C "$dir" update-ref refs/remotes/origin/main "$(git -C "$dir" rev-parse main)"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 0 ]] && [[ "$out" == *"untouched"* ]]; then
    pass "untouched const line exits 0 even when main is ahead"
else
    fail "untouched case: rc=$rc out=$out"
fi

# --- case 4: lowered version ---------------------------------------------
read -r dir sha <<< "$(make_repo 10 "$(version_line 9)")"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 1 ]] && [[ "$out" == *"lowers PROTO_VERSION"* ]]; then
    pass "lowered version exits 1"
else
    fail "lowered case: rc=$rc out=$out"
fi

# --- case 5: unparseable const at the PR head ----------------------------
read -r dir sha <<< "$(make_repo 10 'pub const PROTO_VERSION: u32 = FORTY_FOUR;')"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 1 ]] && [[ "$out" == *"cannot parse a version"* ]]; then
    pass "unparseable const exits 1 rather than passing silently"
else
    fail "unparseable case: rc=$rc out=$out"
fi

# --- case 6: unreachable base ref (fail closed) --------------------------
read -r dir sha <<< "$(make_repo 10 "$(version_line 11)")"
git -C "$dir" update-ref -d refs/remotes/origin/main
git -C "$dir" remote remove origin
out="$( cd "$dir" && PR_HEAD_SHA="$sha" PR_BASE_REF=main PR_REMOTE=origin \
    bash "$GUARD" 2>&1 )"; rc=$?
if [[ $rc -eq 2 ]] && [[ "$out" == *"unable to verify"* ]]; then
    pass "unreachable base ref exits 2 (fail closed)"
else
    fail "unreachable base case: rc=$rc out=$out"
fi

# --- case 7: the merge-ref checkout CI actually gets ----------------------
# `actions/checkout` on a pull_request event checks out refs/pull/N/merge, not
# the branch. This asserts two things at once: the guard still reds on the
# collision from that checkout, AND a diff against the base from there shows
# nothing - which is why the workflow passes PR_HEAD_SHA instead of using HEAD.
read -r dir sha <<< "$(make_repo 10 "$(version_line 11)" 11)"
git -C "$dir" checkout --quiet -b pr-merge main
git -C "$dir" merge --quiet --no-ff -m "merge ref" "$sha" >/dev/null 2>&1
if [[ -z "$(git -C "$dir" diff origin/main HEAD -- "$PROTO_REL")" ]]; then
    pass "merge ref shows no const diff vs base (why PR_HEAD_SHA is required)"
else
    fail "merge ref unexpectedly shows a const diff; premise needs revisiting"
fi
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 1 ]] && [[ "$out" == *"re-bump to v12"* ]]; then
    pass "guard still reds on the collision from a merge-ref checkout"
else
    fail "merge-ref collision case: rc=$rc out=$out"
fi

# --- case 8: the PR merged the updated base in afterwards -----------------
# The branch bumps v10 -> v11, a parallel branch lands the same v11, and the
# branch then merges the updated base into itself (the ordinary "update my
# branch" move). merge-base is now that updated base, so the AGGREGATE diff no
# longer contains the branch's own version-line edit even though the wire change
# is still in the PR. Detection has to be per-commit or the guard reports
# "untouched" and passes the exact collision it exists for.
read -r dir sha <<< "$(make_repo 10 "$(version_line 11)" 11)"
git -C "$dir" checkout --quiet feature
git -C "$dir" merge --quiet --no-edit main >/dev/null 2>&1
merged_sha="$(git -C "$dir" rev-parse HEAD)"
out="$(run_guard "$dir" "$merged_sha")"; rc=$?
if [[ $rc -eq 1 ]] && [[ "$out" == *"re-bump to v12"* ]]; then
    pass "collision survives the branch merging the updated base in"
else
    fail "post-base-merge collision: rc=$rc out=$out"
fi

# --- case 9: merging the base in is not itself a bump ----------------------
# The mirror of case 8: a branch that never touched the const line but merged a
# base that did must stay a no-op. Per-commit detection must not count the merge
# commit's inherited change as the PR's own edit.
dir="$(mktemp -d "$TMP_BASE/repo.XXXXXX")"
git -C "$dir" init --quiet -b main
git -C "$dir" config user.email t@example.com
git -C "$dir" config user.name test
write_proto "$dir" "$(version_line 10)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m base
git -C "$dir" checkout --quiet -b feature
printf '\n// unrelated\n' >> "$dir/$PROTO_REL"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "comment only"
git -C "$dir" checkout --quiet main
write_proto "$dir" "$(version_line 11)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "someone else bumped"
git -C "$dir" checkout --quiet feature
git -C "$dir" merge --quiet --no-edit main >/dev/null 2>&1
merged_sha="$(git -C "$dir" rev-parse HEAD)"
git -C "$dir" remote add origin "$dir"
git -C "$dir" update-ref refs/remotes/origin/main "$(git -C "$dir" rev-parse main)"
out="$(run_guard "$dir" "$merged_sha")"; rc=$?
if [[ $rc -eq 0 ]] && [[ "$out" == *"untouched"* ]]; then
    pass "inheriting someone else's bump via a base merge is not a touch"
else
    fail "inherited-bump case: rc=$rc out=$out"
fi

# --- case 10: a large earlier patch must not swallow the match --------------
# `git log` emits newest-first, so a bump commit on top of a big earlier
# proto.rs commit means the match appears while a lot of output is still
# pending. Piping that into `grep -q` lets grep exit at the match, the producer
# take a SIGPIPE, and `pipefail` turn the whole pipeline non-zero - which reads
# as "no match" and passes the guard on a real collision. The bulk here has to
# exceed the pipe buffer (~64K) for the producer to still be writing.
dir="$(mktemp -d "$TMP_BASE/repo.XXXXXX")"
git -C "$dir" init --quiet -b main
git -C "$dir" config user.email t@example.com
git -C "$dir" config user.name test
write_proto "$dir" "$(version_line 10)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "base v10"
git -C "$dir" checkout --quiet -b feature
# Earlier feature commit: a big wire-format change, no version touch.
{
    echo ""
    for i in $(seq 1 8000); do
        echo "// wire shape line $i - padding to exceed the pipe buffer for real"
    done
} >> "$dir/$PROTO_REL"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "big wire change"
# Later feature commit: the bump itself.
perl -pi -e 's/PROTO_VERSION: u32 = 10;/PROTO_VERSION: u32 = 11;/' "$dir/$PROTO_REL"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "bump to v11"
sha="$(git -C "$dir" rev-parse HEAD)"
git -C "$dir" checkout --quiet main
write_proto "$dir" "$(version_line 11)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "parallel branch landed v11"
git -C "$dir" remote add origin "$dir"
git -C "$dir" update-ref refs/remotes/origin/main "$(git -C "$dir" rev-parse main)"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 1 ]] && [[ "$out" == *"re-bump to v12"* ]]; then
    pass "a large earlier patch does not swallow the version-line match"
else
    fail "large-patch case: rc=$rc out=$out"
fi

echo ""
if [[ $failures -eq 0 ]]; then
    echo "proto-version-bump selftest: all cases passed"
    exit 0
fi
echo "proto-version-bump selftest: $failures case(s) failed" >&2
exit 1
