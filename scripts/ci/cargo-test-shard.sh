#!/usr/bin/env bash
# cargo-test-shard.sh DIR K N - run shard K of N of the integration tests of
# the crate in DIR.
#
# The shards split by test NAME, not by test
# binary: one binary (tests/loop_check.rs) is more than half the suite, so a
# split by binary cannot get the slowest shard under that one file. Every
# test name the suite lists lands in exactly one shard (sorted, dealt
# round-robin), so the shards together run every test. A name that two
# binaries share runs in both, in the same shard.
#
# Extra arguments after DIR K N go to the test binaries, before the names.
set -euo pipefail

usage="usage: cargo-test-shard.sh DIR K N [test-binary args]"
dir="${1:?$usage}"
k="${2:?$usage}"
n="${3:?$usage}"
shift 3
cd "$dir"

# libtest prints one `<name>: test` line per test under --list.
all=()
while IFS= read -r name; do
  all+=("$name")
done < <(cargo test --test '*' -- --list --format terse | sed -n 's/: test$//p' | LC_ALL=C sort -u)

if [ "${#all[@]}" -eq 0 ]; then
  echo "cargo-test-shard: the suite listed no tests (read the build output above); refusing a green run that ran nothing" >&2
  exit 1
fi

mine=()
for i in "${!all[@]}"; do
  if [ $(( i % n )) -eq $(( k - 1 )) ]; then
    mine+=("${all[$i]}")
  fi
done

echo "cargo-test-shard: shard $k/$n runs ${#mine[@]} of ${#all[@]} test names"
if [ "${#mine[@]}" -eq 0 ]; then
  echo "cargo-test-shard: shard $k/$n has no tests; use fewer shards" >&2
  exit 1
fi

cargo test --test '*' --no-fail-fast -- "$@" --exact "${mine[@]}"
