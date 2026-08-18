#!/usr/bin/env bash
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

matches=$(python3 scripts/ci/graphql-caller-inventory.py)

classified=$(printf '%s\n' "$matches" | awk 'NF {n++} END {print n+0}')
digest=$(printf '%s\n' "$matches" | LC_ALL=C sort | shasum -a 256 | awk '{print $1}')
expected_digest="68015553457f6e7ac6a57b779648c306c5472151df0102a9a9f10125230c1be9"

if [[ "${1:-}" == "--print-digest" ]]; then
  printf 'classified_graphql_callers=%s digest=%s\n' "$classified" "$digest"
  exit 0
fi

if [[ "$classified" -eq 0 ]]; then
  echo "direct-graphql-pr-read: classification instrument matched zero callers" >&2
  exit 1
fi
if ! printf '%s\n' "$matches" | grep -q '^argv-pr|cli/src/fno/pr/_merge.py|'; then
  echo "direct-graphql-pr-read: argv inventory missed _merge.py" >&2
  exit 1
fi
if [[ "$digest" != "$expected_digest" ]]; then
  echo "direct-graphql-pr-read: caller inventory changed; classify the new or removed path" >&2
  echo "classified_graphql_callers=$classified unclassified=1 digest=$digest" >&2
  exit 1
fi

echo "classified_graphql_callers=$classified unclassified=0"
