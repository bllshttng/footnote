#!/usr/bin/env bash
# Self-test for scripts/ci/check-registry-schema-bump.sh
#
# Builds throwaway git repos rather than stubbing git, because every case
# worth testing here is a git topology: the collision only exists because a
# field-set change can land at an equal version without any textual
# conflict. Pattern and hermetic-git discipline follow
# check-proto-version-bump-selftest.sh.
#
# Cases:
#    1. fields unchanged (same toml both sides)          -> exit 0
#    2. field added with a bump                          -> exit 0
#    3. field added without a bump (the fac80578be shape) -> exit 1, both versions
#    4. two branches each bump to the same N+1, different fields -> exit 1
#
# Usage: bash scripts/tests/check-registry-schema-bump-selftest.sh
# Exit 0 = all assertions passed.
set -uo pipefail

export GIT_CONFIG_GLOBAL=/dev/null
export GIT_CONFIG_SYSTEM=/dev/null

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
GUARD="$REPO_ROOT/scripts/ci/check-registry-schema-bump.sh"
SCHEMA_REL="crates/fno-agents/src/registry_schema.toml"

if [[ ! -f "$GUARD" ]]; then
    echo "ERROR: guard script not found: $GUARD" >&2
    exit 1
fi

TMP_BASE="$(mktemp -d)"
trap 'rm -rf "$TMP_BASE"' EXIT

failures=0
pass() { echo "  ok: $1"; }
fail() { echo "  FAIL: $1" >&2; failures=$((failures + 1)); }

write_toml() {
    # $1 repo dir, $2 version, $3 fields line ("NONE" writes no fields key)
    mkdir -p "$1/$(dirname "$SCHEMA_REL")"
    {
        echo "# fixture"
        if [[ "$3" != "NONE" ]]; then
            echo "$3"
        fi
        echo "version = $2"
    } > "$1/$SCHEMA_REL"
}

FIELDS_BASE='fields = ["alpha", "beta"]'
FIELDS_PLUS='fields = ["alpha", "beta", "spawn_id"]'
FIELDS_OTHER='fields = ["alpha", "beta", "gamma"]'

# Repo with main at base fields/version, a feature branch whose tip differs,
# and an optional main advance simulating a parallel branch merged first.
# Echoes "<dir> <head_sha>".
make_repo() {
    local base_v="$1" base_f="$2" head_v="$3" head_f="$4" adv_v="${5:-}" adv_f="${6:-}"
    local dir
    dir="$(mktemp -d "$TMP_BASE/repo.XXXXXX")"
    git -C "$dir" init --quiet -b main
    git -C "$dir" config user.email t@example.com
    git -C "$dir" config user.name test
    write_toml "$dir" "$base_v" "$base_f"
    git -C "$dir" add -A
    git -C "$dir" commit --quiet -m base
    git -C "$dir" checkout --quiet -b feature
    write_toml "$dir" "$head_v" "$head_f"
    git -C "$dir" add -A
    git -C "$dir" commit --quiet --allow-empty -m feature
    local head_sha
    head_sha="$(git -C "$dir" rev-parse HEAD)"
    git -C "$dir" checkout --quiet main
    if [[ -n "$adv_v" ]]; then
        write_toml "$dir" "$adv_v" "$adv_f"
        git -C "$dir" add -A
        git -C "$dir" commit --quiet -m advance
    fi
    git -C "$dir" remote add origin "$dir"
    git -C "$dir" fetch --quiet origin main
    git -C "$dir" update-ref refs/remotes/origin/main "$(git -C "$dir" rev-parse main)"
    printf '%s %s\n' "$dir" "$head_sha"
}

run_guard() {
    ( cd "$1" && PR_HEAD_SHA="$2" PR_BASE_REF=main PR_REMOTE=origin \
        bash "$GUARD" 2>&1 )
}

# --- case 1: fields unchanged ------------------------------------------------
read -r dir sha <<< "$(make_repo 36 "$FIELDS_BASE" 36 "$FIELDS_BASE")"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 0 ]] && [[ "$out" == *"fields unchanged; nothing to check"* ]]; then
    pass "fields unchanged exits 0"
else
    fail "fields unchanged: rc=$rc out=$out"
fi

# --- case 2: field added with a bump -----------------------------------------
read -r dir sha <<< "$(make_repo 36 "$FIELDS_BASE" 37 "$FIELDS_PLUS")"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 0 ]] && [[ "$out" == *"v36 -> v37"* ]]; then
    pass "field added with a bump exits 0"
else
    fail "field added with a bump: rc=$rc out=$out"
fi

# --- case 3: field added WITHOUT a bump (the fac80578be shape) ---------------
read -r dir sha <<< "$(make_repo 36 "$FIELDS_BASE" 36 "$FIELDS_PLUS")"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 1 ]] && [[ "$out" == *"v36"* ]] \
    && [[ "$out" == *"Added at head:"* ]] && [[ "$out" == *"spawn_id"* ]]; then
    pass "field added without a bump exits 1 and names versions and the diff"
else
    fail "no-bump case: rc=$rc out=$out"
fi

# --- case 4: two branches each bump to N+1 with different fields -------------
# The feature branch took v37 with spawn_id; a parallel branch took v37 with
# gamma and merged first. State comparison catches what a patch scan cannot:
# both sides made a "bump", the merged base is v37, and the PR's field set
# still differs from the base tip's at an equal version.
read -r dir sha <<< "$(make_repo 36 "$FIELDS_BASE" 37 "$FIELDS_PLUS" 37 "$FIELDS_OTHER")"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 1 ]] && [[ "$out" == *"v37"* ]]; then
    pass "equal parallel bumps with different fields exits 1"
else
    fail "parallel-bump case: rc=$rc out=$out"
fi

# --- case 5: base tip has no fields key yet (the introducing PR) -------------
# This PR's own shape: the base toml predates the fields key. The no-match
# grep used to fail the pipelined read under pipefail and exit 2 silently;
# absence is the empty field set and exits 0 with the reason.
read -r dir sha <<< "$(make_repo 36 NONE 36 "$FIELDS_BASE")"
out="$(run_guard "$dir" "$sha")"; rc=$?
if [[ $rc -eq 0 ]] && [[ "$out" == *"no baseline fields at base tip"* ]]; then
    pass "no baseline fields at base tip exits 0"
else
    fail "introducing-PR case: rc=$rc out=$out"
fi

if [[ $failures -eq 0 ]]; then
    echo "check-registry-schema-bump-selftest: all 5 cases pass"
    exit 0
fi
echo "check-registry-schema-bump-selftest: $failures failure(s)" >&2
exit 1
