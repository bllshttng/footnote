#!/usr/bin/env bash
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
script="$repo_root/scripts/diagnostics/graphql-quota-soak.py"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
now=$(date -u +%Y-%m-%dT%H:%M:%SZ)

mkdir -p "$tmp/good" "$tmp/bad"
printf '%s\n' "{\"ended_at\":\"$now\",\"duration_seconds\":3600,\"samples\":60,\"floor\":200,\"min_remaining\":201,\"min_live_workers\":15,\"coverage\":\"covered\",\"reviewed_count\":1,\"head_sha\":\"abc\",\"coverage_head_sha\":\"abc\",\"settled\":true}" > "$tmp/good/receipt.json"
printf '%s\n' "{\"ended_at\":\"$now\",\"duration_seconds\":3600,\"samples\":60,\"floor\":200,\"min_remaining\":200,\"min_live_workers\":15,\"coverage\":\"unknown\",\"reviewed_count\":0,\"head_sha\":\"abc\",\"coverage_head_sha\":\"def\",\"settled\":true}" > "$tmp/bad/receipt.json"

good=$(python3 "$script" --check-latest --receipt-dir "$tmp/good" --min-seconds 3600 --max-age-hours 24)
grep -q 'settled=true' <<<"$good"
grep -q 'min_remaining=201' <<<"$good"
grep -q 'coverage=covered' <<<"$good"

set +e
bad=$(python3 "$script" --check-latest --receipt-dir "$tmp/bad" --min-seconds 3600 --max-age-hours 24 2>&1)
rc=$?
set -e
[[ $rc -ne 0 ]]
grep -q 'min_remaining>200' <<<"$bad"
grep -q 'coverage=covered' <<<"$bad"
grep -q 'reviewed_count>0' <<<"$bad"
grep -q 'coverage_head_sha=head_sha' <<<"$bad"

echo "graphql-quota-soak tests: PASS"
