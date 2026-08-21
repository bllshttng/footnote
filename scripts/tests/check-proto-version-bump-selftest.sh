#!/usr/bin/env bash
# Self-test for scripts/ci/check-proto-version-bump.sh
#
# Builds throwaway git repos rather than stubbing git, because every case worth
# testing here IS a git topology: the collision case only exists because two
# identical one-line changes merge without a conflict, and no amount of
# function-level string testing reproduces that.
#
# Cases:
#    1. clean bump (base v10, PR v11)                    -> exit 0
#    2. identical parallel bump (base v11, PR v11)       -> exit 1, "re-bump to v12"
#   2b. those identical bumps really do merge clean      -> the guard's premise
#    3. const line untouched, base ahead                 -> exit 0
#    4. lowered version                                  -> exit 1
#    5. unparseable const at the PR head                 -> exit 1 (never a silent pass)
#   5b. unparseable const at the BASE                    -> exit 1
#   5c. const line absent entirely at the PR head        -> exit 1
#    6. unreachable base ref                             -> exit 2 (fail closed)
#    7. merge-ref checkout (what CI actually gets)       -> still exit 1
#    8. branch merged the updated base in afterwards     -> still exit 1
#    9. inherited someone else's bump via a base merge   -> exit 0 (not a touch)
#   10. bump above a large earlier patch (SIGPIPE trap)  -> still exit 1
#   11. non-main base ref via PR_BASE_REF                -> exit 0
#   12. empty PR_HEAD_SHA under CI                       -> exit 2 (refused)
#   13. unknown arg                                      -> exit 2 (refused)
#   14. untouched PR + unlocatable base const            -> exit 1 (order guard)
#   15. bump then revert inside the PR                   -> exit 1, revert guidance
#   16. failed fetch over a stale tracking ref           -> exit 2 (refused)
#   17. fetch REFRESHES a stale tracking ref             -> exit 1 (positive control)
#   18. NOVEL merge resolution (differs from both)       -> exit 1 (pairs with 9)
#   19. leading-zero version reads as decimal            -> exit 0
#   20. grep itself errors                               -> exit 2 (fail closed)
# Case 1 also asserts the run leaves the caller's repo unshallowed.
#
# Usage: bash scripts/tests/check-proto-version-bump-selftest.sh
# Exit 0 = all assertions passed.
#
# HOW THIS RUNS IN CI: auto-discovered, no registry entry. `scripts/tests/*.sh` is
# in _SMOKE_HARNESS_GLOBS (cli/src/fno/test_cmd.py), so the smoke runner picks it
# up, and cli-ci.yml runs `fno-py doctor test smoke` on `scripts/**`. Confirm with
# `fno doctor test smoke --list | grep proto-version-bump` - worth stating, because the
# glob makes the wiring invisible to a grep for this filename.
set -uo pipefail

# Hermetic git: the fixtures make dozens of commits, and an ambient
# `commit.gpgsign = true` with an unusable key fails every one of them (measured:
# 10 of 12 cases red). Contributor config must not decide whether this passes.
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_CONFIG_SYSTEM=/dev/null

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

# Assert a real merge commit landed in $1 at HEAD, labelled $2.
#
# Load-bearing, not defensive noise. The post-base-merge cases assert the SAME
# rc+message that the plain collision case asserts, so a merge that silently did
# not happen leaves them green while testing nothing about per-commit detection.
require_merge_commit() {
    local dir="$1" label="$2" parents
    parents="$(git -C "$dir" rev-list --parents -n1 HEAD | wc -w | tr -d ' ')"
    if [[ "$parents" -ne 3 ]]; then
        fail "$label: fixture merge did not create a merge commit (parents=$((parents - 1)))"
        return 1
    fi
    return 0
}

run_guard() {
    # $1 = repo dir, $2 = head sha; echoes output, returns guard's exit code
    ( cd "$1" && PR_HEAD_SHA="$2" PR_BASE_REF=main PR_REMOTE=origin \
        bash "$GUARD" 2>&1 )
}

# --- case 1: clean bump, and the run leaves the repo intact ----------------
read -r dir sha <<< "$(make_repo 10 "$(version_line 11)")"
commits_before="$(git -C "$dir" rev-list --count HEAD)"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 0 ]] && [[ "$out" == *"v10 -> v11"* ]]; then
    pass "clean bump exits 0"
else
    fail "clean bump: rc=$rc out=$out"
fi
# The guard fetches on every run, and `--depth` there would write .git/shallow
# and truncate the caller's repository (measured: 5 commits -> 1). The header
# calls that out; nothing asserted it until here. A read-only question must not
# mutate the repo it is asked about.
commits_after="$(git -C "$dir" rev-list --count HEAD)"
if [[ ! -f "$dir/.git/shallow" ]] && [[ "$commits_before" == "$commits_after" ]]; then
    pass "the run leaves the repo unshallowed and its history intact"
else
    fail "guard truncated the caller's repo: shallow=$([ -f "$dir/.git/shallow" ] && echo yes || echo no) commits=$commits_before->$commits_after"
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
# Match the base-resolution message specifically. "unable to verify" alone is
# carried by three different exit-2 paths (bad base, bad head, unreadable
# history), so this case would stay green while the failure it targets stopped
# working, as long as ANY of them fired.
if [[ $rc -eq 2 ]] && [[ "$out" == *"cannot resolve origin/main"* ]]; then
    pass "unreachable base ref exits 2 (fail closed)"
else
    fail "unreachable base case: rc=$rc out=$out"
fi

# --- case 7: the merge-ref checkout CI actually gets ----------------------
# Asserts two things at once: the guard still reds on the collision from a
# merge-ref checkout, AND a diff against the base from there shows nothing -
# which is why PR_HEAD_SHA exists. Rationale lives in the guard's header.
read -r dir sha <<< "$(make_repo 10 "$(version_line 11)" 11)"
git -C "$dir" checkout --quiet -b pr-merge main
git -C "$dir" merge --quiet --no-ff -m "merge ref" "$sha" >/dev/null 2>&1
require_merge_commit "$dir" "merge-ref fixture" || true
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
require_merge_commit "$dir" "post-base-merge fixture" || true
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
require_merge_commit "$dir" "inherited-bump fixture" || true
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
# The SIGPIPE-under-pipefail trap; the guard's match site explains it. `git log`
# emits newest-first, so the bump matches while a lot of output is still pending.
# The bulk has to exceed the pipe buffer (~64K) for the producer to still be
# writing when a piped `grep -q` would exit.
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

# --- case 5b: unparseable const at the BASE -------------------------------
# Case 5 only corrupts the head. The base leg has its own read_version call, and
# an unreadable base must fail loud there too rather than compare against junk.
dir="$(mktemp -d "$TMP_BASE/repo.XXXXXX")"
git -C "$dir" init --quiet -b main
git -C "$dir" config user.email t@example.com
git -C "$dir" config user.name test
write_proto "$dir" 'pub const PROTO_VERSION: u32 = NOT_A_NUMBER;'
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "base with a bad const"
git -C "$dir" checkout --quiet -b feature
write_proto "$dir" "$(version_line 11)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "bump"
sha="$(git -C "$dir" rev-parse HEAD)"
git -C "$dir" checkout --quiet main
git -C "$dir" remote add origin "$dir"
git -C "$dir" update-ref refs/remotes/origin/main "$(git -C "$dir" rev-parse main)"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 1 ]] && [[ "$out" == *"cannot parse a version"* ]] && [[ "$out" == *"tip"* ]]; then
    pass "unparseable const at the base exits 1, naming the base"
else
    fail "base-unparseable case: rc=$rc out=$out"
fi

# --- case 5c: const line absent entirely ----------------------------------
# Distinct from unparseable: nothing matches the const grep at all. Must not be
# read as "no bump needed".
read -r dir sha <<< "$(make_repo 10 '// the const was deleted outright')"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 1 ]] && [[ "$out" == *"no 'pub const PROTO_VERSION' line"* ]]; then
    pass "an absent const line exits 1 rather than passing silently"
else
    fail "absent-const case: rc=$rc out=$out"
fi

# --- case 11: a base ref that is not main ---------------------------------
# Every other case hardcodes main, so nothing pins the PR_BASE_REF plumbing and
# a regression there would be invisible.
dir="$(mktemp -d "$TMP_BASE/repo.XXXXXX")"
git -C "$dir" init --quiet -b develop
git -C "$dir" config user.email t@example.com
git -C "$dir" config user.name test
write_proto "$dir" "$(version_line 10)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "develop base v10"
git -C "$dir" checkout --quiet -b feature
write_proto "$dir" "$(version_line 11)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "bump"
sha="$(git -C "$dir" rev-parse HEAD)"
git -C "$dir" checkout --quiet develop
git -C "$dir" remote add origin "$dir"
git -C "$dir" update-ref refs/remotes/origin/develop "$(git -C "$dir" rev-parse develop)"
out="$( cd "$dir" && PR_HEAD_SHA="$sha" PR_BASE_REF=develop PR_REMOTE=origin \
    bash "$GUARD" 2>&1 )"; rc=$?
if [[ $rc -eq 0 ]] && [[ "$out" == *"v10 -> v11"* ]] && [[ "$out" == *"origin/develop"* ]]; then
    pass "a non-main base ref is honored end to end"
else
    fail "non-main base case: rc=$rc out=$out"
fi

# --- case 12: an empty PR_HEAD_SHA under CI is refused --------------------
# Not a silent-pass case but the refusal that PREVENTS one: falling back to HEAD
# in CI means reading the merge ref, which cannot show the PR's own bump.
read -r dir sha <<< "$(make_repo 10 "$(version_line 11)")"
out="$( cd "$dir" && CI=1 PR_HEAD_SHA= PR_BASE_REF=main PR_REMOTE=origin \
    bash "$GUARD" 2>&1 )"; rc=$?
if [[ $rc -eq 2 ]] && [[ "$out" == *"PR_HEAD_SHA is empty under CI"* ]]; then
    pass "an empty PR_HEAD_SHA under CI is refused, not defaulted to HEAD"
else
    fail "empty-head-under-CI case: rc=$rc out=$out"
fi

# --- case 13: an unknown arg is refused, not ignored ----------------------
# Silently accepting `--base other` while checking main is the quiet
# misconfiguration this guard exists to prevent.
# No env needed: the refusal happens before the first git call.
out="$( cd "$dir" && bash "$GUARD" --base other 2>&1 )"; rc=$?
if [[ $rc -eq 2 ]] && [[ "$out" == *"unknown arg"* ]]; then
    pass "an unknown arg is refused rather than ignored"
else
    fail "unknown-arg case: rc=$rc out=$out"
fi

# --- case 14: untouched PR + a base const that cannot be located ----------
# The guard reads the base version BEFORE deciding "untouched", and this is the
# case that notices if that order is ever reverted. Both greps anchor `pub const`
# at column 0, so a const indented into a `mod` matches nothing: with the old
# order the guard printed a pass for every PR forever. Without this case,
# reverting the reorder leaves every other case green.
dir="$(mktemp -d "$TMP_BASE/repo.XXXXXX")"
git -C "$dir" init --quiet -b main
git -C "$dir" config user.email t@example.com
git -C "$dir" config user.name test
mkdir -p "$dir/$(dirname "$PROTO_REL")"
cat > "$dir/$PROTO_REL" <<'EOF'
// fixture: the const moved inside a mod, so it is no longer at column 0
mod wire {
    pub const PROTO_VERSION: u32 = 10;
}
EOF
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "base with an indented const"
git -C "$dir" checkout --quiet -b feature
printf '\n// an unrelated edit, the const line is untouched\n' >> "$dir/$PROTO_REL"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "unrelated"
sha="$(git -C "$dir" rev-parse HEAD)"
git -C "$dir" checkout --quiet main
git -C "$dir" remote add origin "$dir"
git -C "$dir" update-ref refs/remotes/origin/main "$(git -C "$dir" rev-parse main)"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 1 ]] && [[ "$out" == *"no 'pub const PROTO_VERSION' line"* ]] \
   && [[ "$out" == *"tip"* ]]; then
    pass "an unlocatable base const fails loud even when the PR is untouched"
else
    fail "untouched-unlocatable case: rc=$rc out=$out"
fi

# --- case 15: bump then revert inside the PR ------------------------------
# Reaches the equal-version branch without any parallel branch existing, which
# is why that message offers its cause as a likely cause instead of asserting it.
# Pins the revert guidance, which nothing else asserts.
dir="$(mktemp -d "$TMP_BASE/repo.XXXXXX")"
git -C "$dir" init --quiet -b main
git -C "$dir" config user.email t@example.com
git -C "$dir" config user.name test
write_proto "$dir" "$(version_line 10)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "base v10"
git -C "$dir" checkout --quiet -b feature
write_proto "$dir" "$(version_line 11)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "bump to v11"
write_proto "$dir" "$(version_line 10)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "revert the bump"
sha="$(git -C "$dir" rev-parse HEAD)"
git -C "$dir" checkout --quiet main
git -C "$dir" remote add origin "$dir"
git -C "$dir" update-ref refs/remotes/origin/main "$(git -C "$dir" rev-parse main)"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 1 ]] && [[ "$out" == *"unchanged for any reason"* ]]; then
    pass "bump-then-revert reds and is told how to resolve it"
else
    fail "bump-then-revert case: rc=$rc out=$out"
fi

# --- case 16: a failed fetch with a stale tracking ref must refuse --------
# The nastiest false pass this guard had: a tracking ref pinned at v10 while the
# real base holds v11 and the remote is unreachable. rev-parse succeeds against
# the stale ref, so tolerating a failed fetch printed "OK (v10 -> v11)" on a real
# collision. Not fetching and not knowing are the same state.
dir="$(mktemp -d "$TMP_BASE/repo.XXXXXX")"
git -C "$dir" init --quiet -b main
git -C "$dir" config user.email t@example.com
git -C "$dir" config user.name test
write_proto "$dir" "$(version_line 10)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "base v10"
git -C "$dir" checkout --quiet -b feature
write_proto "$dir" "$(version_line 11)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "bump to v11"
sha="$(git -C "$dir" rev-parse HEAD)"
git -C "$dir" checkout --quiet main
git -C "$dir" update-ref refs/remotes/origin/main "$(git -C "$dir" rev-parse main)"
write_proto "$dir" "$(version_line 11)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "parallel branch landed v11"
git -C "$dir" remote add origin "$TMP_BASE/definitely-not-a-repo.git"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 2 ]] && [[ "$out" == *"cannot be confirmed current"* ]]; then
    pass "a failed fetch over a stale tracking ref refuses instead of passing"
else
    fail "stale-tracking-ref case: rc=$rc out=$out"
fi

# --- case 17: the fetch must actually REFRESH a stale tracking ref --------
# The positive control every other case lacks. They all pin the tracking ref to
# the current base before running, so the fetch has nothing to do and a fetch
# that silently stopped updating the ref would go unnoticed. Here the ref starts
# stale at v10 while the base really holds v11, over a WORKING remote: only a
# fetch that remaps refs/remotes/origin/main sees the collision.
#
# This is the case that catches a bare `git fetch <remote> <branch>`, whose
# tracking-ref update is opportunistic and does not happen under a narrowed
# remote.origin.fetch refspec.
upstream="$(mktemp -d "$TMP_BASE/up.XXXXXX")"
git -C "$upstream" init --quiet -b main
git -C "$upstream" config user.email t@example.com
git -C "$upstream" config user.name test
write_proto "$upstream" "$(version_line 10)"
git -C "$upstream" add -A && git -C "$upstream" commit --quiet -m "base v10"

dir="$(mktemp -d "$TMP_BASE/repo.XXXXXX")"
git -C "$dir" clone --quiet "$upstream" . 2>/dev/null || git clone --quiet "$upstream" "$dir"
git -C "$dir" config user.email t@example.com
git -C "$dir" config user.name test
git -C "$dir" checkout --quiet -b feature
write_proto "$dir" "$(version_line 11)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "bump to v11"
sha="$(git -C "$dir" rev-parse HEAD)"
git -C "$dir" checkout --quiet main
# Upstream independently reaches v11; the clone's tracking ref is still v10.
write_proto "$upstream" "$(version_line 11)"
git -C "$upstream" add -A && git -C "$upstream" commit --quiet -m "parallel landed v11"
# Narrow the refspec so a bare `fetch origin main` would NOT remap the ref.
git -C "$dir" config remote.origin.fetch '+refs/heads/nothing:refs/remotes/origin/nothing'
# Anchored to the PROTO_VERSION line: the fixture also carries a SOMETHING_ELSE
# const, and an unanchored match returns both numbers.
stale_v="$(git -C "$dir" show origin/main:"$PROTO_REL" \
    | sed -n 's/^pub const PROTO_VERSION[^=]*=[[:space:]]*\([0-9]\{1,\}\).*/\1/p')"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ "$stale_v" == "10" ]] && [[ $rc -eq 1 ]] && [[ "$out" == *"re-bump to v12"* ]]; then
    pass "the fetch refreshes a stale tracking ref (collision still caught)"
else
    fail "stale-ref-refresh case: stale_v=$stale_v rc=$rc out=$out"
fi

# --- case 18: a NOVEL merge resolution is scanned ------------------------
# The other half of case 9. A merge emits no patch by default, which is what keeps
# an inherited bump from counting - but it also hid a resolution differing from
# BOTH parents. Here the merge resolves the version DOWN to v9 while the base
# holds v12, so nothing but a combined diff sees it.
dir="$(mktemp -d "$TMP_BASE/repo.XXXXXX")"
git -C "$dir" init --quiet -b main
git -C "$dir" config user.email t@example.com
git -C "$dir" config user.name test
write_proto "$dir" "$(version_line 10)"
echo x > "$dir/other.txt"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "base v10"
git -C "$dir" checkout --quiet -b feature
echo y >> "$dir/other.txt"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "unrelated edit"
git -C "$dir" checkout --quiet main
write_proto "$dir" "$(version_line 12)"
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "main reaches v12"
git -C "$dir" checkout --quiet feature
git -C "$dir" merge --no-commit main >/dev/null 2>&1 || true
write_proto "$dir" "$(version_line 9)"   # novel: differs from both parents
git -C "$dir" add -A && git -C "$dir" commit --quiet -m "merge main, resolve to v9"
require_merge_commit "$dir" "novel-resolution fixture" || true
sha="$(git -C "$dir" rev-parse HEAD)"
git -C "$dir" checkout --quiet main
git -C "$dir" remote add origin "$dir"
git -C "$dir" update-ref refs/remotes/origin/main "$(git -C "$dir" rev-parse main)"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 1 ]] && [[ "$out" == *"lowers PROTO_VERSION"* ]]; then
    pass "a novel merge resolution is scanned, not skipped as inherited"
else
    fail "novel-resolution case: rc=$rc out=$out"
fi

# --- case 19: a leading-zero version is read as decimal -------------------
# Rust reads `= 010;` as ten; bash arithmetic reads it as octal eight. Without the
# `10#` forcing, a v9 base against an 010 head flips the verdict to "lowers".
read -r dir sha <<< "$(make_repo 9 "$(version_line 010)")"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 0 ]] && [[ "$out" == *"v9 -> v010"* ]]; then
    pass "a leading-zero version compares as decimal, not octal"
else
    fail "leading-zero case: rc=$rc out=$out"
fi

# --- case 20: a grep that ERRORS is not read as "no match" ---------------
# The one branch where turning `exit 2` into `exit 0` would leave every other case
# green. A stub fails only the `-qE` scan, so read_version's plain `grep -E` still
# works and the run reaches the patch scan.
read -r dir sha <<< "$(make_repo 10 "$(version_line 11)" 11)"
stubdir="$(mktemp -d "$TMP_BASE/stub.XXXXXX")"
cat > "$stubdir/grep" <<'EOF'
#!/usr/bin/env bash
for a in "$@"; do [[ "$a" == "-qE" ]] && exit 2; done
exec /usr/bin/grep "$@"
EOF
chmod +x "$stubdir/grep"
out="$( cd "$dir" && PATH="$stubdir:$PATH" PR_HEAD_SHA="$sha" PR_BASE_REF=main \
    PR_REMOTE=origin bash "$GUARD" 2>&1 )"; rc=$?
if [[ $rc -eq 2 ]] && [[ "$out" == *"cannot scan this PR's patch"* ]]; then
    pass "a grep error fails closed instead of reading as untouched"
else
    fail "grep-error case: rc=$rc out=$out"
fi

echo ""
if [[ $failures -eq 0 ]]; then
    echo "proto-version-bump selftest: all cases passed"
    exit 0
fi
echo "proto-version-bump selftest: $failures case(s) failed" >&2
exit 1
