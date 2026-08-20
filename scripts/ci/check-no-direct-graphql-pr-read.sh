#!/usr/bin/env bash
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

matches=$(python3 scripts/ci/graphql-caller-inventory.py)

classified=$(printf '%s\n' "$matches" | awk 'NF {n++} END {print n+0}')
unclassified=$(printf '%s\n' "$matches" | awk -F'|' '$1 == "unclassified" {n++} END {print n+0}')
digest=$(printf '%s\n' "$matches" | LC_ALL=C sort | shasum -a 256 | awk '{print $1}')
expected_digest="63ad7c8054a29a741db49339c1b15cd333a1fbf15a09178eae4a11cbdd713bf4"

if [[ "${1:-}" == "--print-digest" ]]; then
  printf 'classified_graphql_callers=%s digest=%s\n' "$classified" "$digest"
  exit 0
fi

if [[ "$classified" -eq 0 ]]; then
  echo "direct-graphql-pr-read: classification instrument matched zero callers" >&2
  exit 1
fi
if [[ "$unclassified" -ne 0 ]]; then
  printf '%s\n' "$matches" | awk -F'|' '$1 == "unclassified"' >&2
  echo "direct-graphql-pr-read: executable callers lack a routing disposition" >&2
  exit 1
fi
if ! printf '%s\n' "$matches" | grep -q '^fno-process-proxy|argv-pr|cli/src/fno/pr/_merge.py|'; then
  echo "direct-graphql-pr-read: argv inventory missed _merge.py" >&2
  exit 1
fi
# Pin the BOUNDARY, never the line that happens to express it today. Both of
# the first two pins broke on ordinary edits that changed nothing about the
# boundary: `worker_environment(os.environ)` became `worker_environment(base)`,
# and the `gh_bin` default was wrapped across two lines. A whole-line grep then
# reports "a named enforcement boundary is missing" for a boundary that is
# present and working, which is a false red nobody can act on. Match the
# assignment and the callee, or the binary name, and let formatting move.
if ! grep -q 'protected = worker_environment(' cli/src/fno/cli.py \
    || ! grep -q 'Command::new("fno-gh-coverage")' crates/fno-agents/src/finalize.rs \
    || ! grep -q '"fno-gh-loopcheck"' crates/fno-agents/src/loopcheck.rs; then
  echo "direct-graphql-pr-read: a named enforcement boundary is missing" >&2
  exit 1
fi
if [[ "$digest" != "$expected_digest" ]]; then
  echo "direct-graphql-pr-read: caller inventory changed; classify the new or removed path" >&2
  echo "classified_graphql_callers=$classified unclassified=$unclassified digest=$digest" >&2
  exit 1
fi

echo "classified_graphql_callers=$classified unclassified=0"
