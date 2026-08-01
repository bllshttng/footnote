#!/usr/bin/env bash
# Validate the machine-readable terminal record from `/fno:review peer` and
# convert it into the one head-pinned attestation consumed by loop-check.
set -euo pipefail

review_file="${1:?usage: consume-peer-verdict.sh <review-file>}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

emit() {
  bash "$script_dir/emit-attestation.sh" peer "$1"
}

reject() {
  echo "consume-peer-verdict: $1; peer gate remains unmet" >&2
  if ! emit fail; then
    echo "consume-peer-verdict: could not record the failed verdict" >&2
    exit 2
  fi
  exit 1
}

[[ -f "$review_file" ]] || reject "review output file not found: $review_file"
last_line="$(awk 'NF { line=$0 } END { print line }' "$review_file")"
[[ -n "$last_line" ]] || reject "review output is empty"
prefix="fno-peer-verdict: "
[[ "$last_line" == "$prefix"* ]] || reject "missing terminal fno-peer-verdict record"

record="${last_line#"$prefix"}"
if ! jq -e '
  type == "object" and
  (keys | sort) == ["blocking_findings", "verdict"] and
  (.verdict == "clean" or .verdict == "blocked") and
  (.blocking_findings | type == "number" and . >= 0 and floor == .)
' >/dev/null 2>&1 <<<"$record"; then
  reject "terminal verdict record is malformed"
fi

verdict="$(jq -r '.verdict' <<<"$record")"
declared="$(jq -r '.blocking_findings' <<<"$record")"
observed="$(grep -Ec '^[[:space:]]*P[12]([[:space:]:-]|$)' "$review_file" || true)"

[[ "$observed" -eq "$declared" ]] || reject \
  "terminal record declares $declared blocking finding(s), but output contains $observed"

case "$verdict:$declared" in
  clean:0)
    emit pass
    ;;
  blocked:0)
    reject "blocked verdict declares zero blocking findings"
    ;;
  blocked:*)
    emit fail
    echo "consume-peer-verdict: peer reported $declared blocking finding(s)" >&2
    exit 1
    ;;
  clean:*)
    reject "clean verdict contradicts $declared blocking finding(s)"
    ;;
esac
