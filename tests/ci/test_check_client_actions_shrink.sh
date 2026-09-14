#!/usr/bin/env bash
# tests/ci/test_check_client_actions_shrink.sh
#
# Exercises scripts/ci/check-client-actions-shrink.sh against scratch repos: a
# local origin holding main, and a feature branch whose diff is the case under
# test. Every case asserts the exit code AND a line only that outcome prints,
# so a gate that skipped a path cannot pass by printing nothing.
#
# Run: bash tests/ci/test_check_client_actions_shrink.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$(cd "${SCRIPT_DIR}/../.." && pwd)/scripts/ci/check-client-actions-shrink.sh"
[[ -f "$GATE" ]] || { echo "gate not found at $GATE" >&2; exit 1; }

# The caller's git config (signing, hooks) must not reach the scratch repos.
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
unset PR_BASE_REF PR_REMOTE ACTIONS_BASE_SHA

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

PASS=0; FAIL=0

const_file() { # $@ = action tokens
  mkdir -p crates/fno-agents/src/bin
  {
    echo 'const ALL_CLIENT_ACTIONS: &[&str] = &['
    for t in "$@"; do echo "    \"$t\","; done
    echo '];'
  } >crates/fno-agents/src/bin/client.rs
}

# fresh: a new origin whose main holds the base const, and a feature branch
# checked out on top of it.
fresh() {
  rm -rf "$TMP/origin.git" "$TMP/repo"
  git init -q --bare "$TMP/origin.git"
  git init -q "$TMP/repo"
  cd "$TMP/repo" || exit 1
  git symbolic-ref HEAD refs/heads/main
  const_file keep-a keep-b keep-c
  git add -A && git commit -qm base
  git remote add origin "$TMP/origin.git"
  git push -q origin main
  git checkout -q -b feature
}

commit() { git add -A && git commit -qm change "$@"; }

# check <label> <want_exit> <marker> [VAR=value...]
check() {
  local label="$1" want="$2" marker="$3" out got
  shift 3
  out="$(env "$@" bash "$GATE" 2>&1)"; got=$?
  if [[ "$got" -eq "$want" && "$out" == *"$marker"* ]]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s\n  want exit %s with: %s\n  got exit %s:\n%s\n' "$label" "$want" "$marker" "$got" "$out"
  fi
}

# --- additions are refused ------------------------------------------------------
fresh; const_file keep-a keep-b keep-c lane-heal; commit
check 'an added action is refused' 1 'added: lane-heal'

# --- removals are banked --------------------------------------------------------
fresh; const_file keep-a keep-c; commit
check 'a removed action is banked' 0 'banked: keep-b'

# --- a swap counts as an addition ----------------------------------------------
fresh; const_file keep-a keep-b keep-x; commit
check 'a swap refuses as an addition' 1 'added: keep-x'

# --- a renamed or moved const cannot bypass -------------------------------------
fresh
mkdir -p crates/fno-agents/src/bin
sed 's/const ALL_CLIENT_ACTIONS:/const CLIENT_ACTIONS:/' crates/fno-agents/src/bin/client.rs >tmp.rs
mv tmp.rs crates/fno-agents/src/bin/client.rs
commit
check 'a renamed const in head refuses' 2 'head side lost const ALL_CLIENT_ACTIONS'

# --- an unresolvable base refuses -----------------------------------------------
fresh
git remote remove origin
check 'an unresolvable base refuses' 2 'cannot resolve a base'

# --- unchanged passes with no tokens named --------------------------------------
fresh; commit --allow-empty
check 'an unchanged list passes' 0 'the action list only shrank'

# --- the push alarm pins an explicit sha ----------------------------------------
fresh; const_file keep-a keep-b; commit
BASE_SHA="$(git rev-parse origin/main)"
check 'an explicit base sha diffs that tip' 0 'banked: keep-c' "ACTIONS_BASE_SHA=$BASE_SHA"
# shellcheck disable=SC2034 # reused in the next check call
check 'the all-zeros sha counts as unset (falls back to the merge base)' 0 'the action list only shrank' ACTIONS_BASE_SHA=0000000000000000000000000000000000000000
check 'an unresolvable explicit sha refuses' 2 'ACTIONS_BASE_SHA deadbeef does not resolve' ACTIONS_BASE_SHA=deadbeef

echo "PASS=$PASS FAIL=$FAIL"
[[ "$FAIL" -eq 0 ]]
