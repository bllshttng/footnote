#!/usr/bin/env bash
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

matches=$(RIPGREP_CONFIG_PATH= rg -uu --with-filename --no-line-number -o \
  --glob '!**/tests/**' \
  --glob '!**/target/**' \
  --glob '!**/.git/**' \
  --glob '!scripts/ci/check-no-direct-graphql-pr-read.sh' \
  -e 'gh pr view.*--json' \
  -e 'gh pr checks' \
  -e 'gh api graphql' \
  cli/src crates/fno-agents/src hooks skills scripts 2>/dev/null || true)

classified=$(printf '%s\n' "$matches" | awk 'NF {n++} END {print n+0}')
digest=$(printf '%s\n' "$matches" | LC_ALL=C sort | shasum -a 256 | awk '{print $1}')
expected_digest="4f6ddb95c44db1380078e062e85c07f2e54611a86051dbe22679f651066e659f"

if [[ "${1:-}" == "--print-digest" ]]; then
  printf 'classified_graphql_callers=%s digest=%s\n' "$classified" "$digest"
  exit 0
fi

if [[ "$classified" -eq 0 ]]; then
  echo "direct-graphql-pr-read: classification instrument matched zero callers" >&2
  exit 1
fi
if [[ "$digest" != "$expected_digest" ]]; then
  echo "direct-graphql-pr-read: caller inventory changed; classify the new or removed path" >&2
  echo "classified_graphql_callers=$classified unclassified=1 digest=$digest" >&2
  exit 1
fi

echo "classified_graphql_callers=$classified unclassified=0"
