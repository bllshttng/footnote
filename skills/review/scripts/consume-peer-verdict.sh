#!/usr/bin/env bash
# Validate the machine-readable terminal record from `/fno:review peer` and
# convert it into the head-pinned attestation consumed by loop-check.
#
# Two terminal-record shapes, one classifier:
#   - current: fno-peer-verdict: {"verdict":"clean|blocked","findings":[...]}
#     The findings array goes through `fno do review classify`, the one shell
#     entry point, and the emitted verdict follows the classified blocking
#     count: pass only when nothing blocking survives.
#   - legacy:  fno-peer-verdict: {"verdict":"clean|blocked","blocking_findings":N}
#     Still accepted because peers in the wild emit it. N unclassifiable
#     records are synthesized, each BLOCKING by the fail-closed rule, so a
#     legacy count can never read as milder than it declared.
set -euo pipefail

review_file="${1:?usage: consume-peer-verdict.sh <review-file>}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

payload_file="$(mktemp -t consume-peer-verdict.XXXXXX)"
trap 'rm -f "$payload_file"' EXIT

emit() {
  # $1 = verdict; $2 (optional) = findings-file for the classified record
  if [[ -n "${2:-}" ]]; then
    bash "$script_dir/emit-attestation.sh" peer "$1" unknown --findings-file "$2"
  else
    bash "$script_dir/emit-attestation.sh" peer "$1"
  fi
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
if jq -e '
  type == "object"
  and (keys | sort) == ["blocking_findings", "verdict"]
  and (.verdict == "clean" or .verdict == "blocked")
  and (.blocking_findings | type == "number" and . >= 0 and floor == .)
' >/dev/null 2>&1 <<<"$record"; then
  # Legacy shape: N declared blocking findings become N unclassifiable
  # records. The classifier's fail-closed rule makes each one BLOCKING, so
  # the synthesized corpus cannot read as milder than the declaration.
  verdict="$(jq -r '.verdict' <<<"$record")"
  declared="$(jq -r '.blocking_findings' <<<"$record")"
  jq -nc --argjson n "$declared" '[range($n) | {}]' > "$payload_file"
elif jq -e '
  type == "object"
  and (keys | sort) == ["findings", "verdict"]
  and (.verdict == "clean" or .verdict == "blocked")
  and (.findings | type == "array")
' >/dev/null 2>&1 <<<"$record"; then
  verdict="$(jq -r '.verdict' <<<"$record")"
  printf '%s' "$record" > "$payload_file"
else
  reject "terminal verdict record is malformed"
fi

# The classified record: the same rule every producer shares. A classify
# failure (a deployment older than the verb) is a rejection, never an
# empty classification.
if ! classified_record="$("${FNO:-fno}" do review classify --findings-file "$payload_file" --emit-record)" 2>/dev/null; then
  reject "classify refused the terminal record (a deployment without 'do review classify' is too old for this script)"
fi
blocking="$(jq -r '.findings_blocking // 1' <<<"$classified_record" 2>/dev/null || echo 1)"
nonblocking="$(jq -r '.findings_nonblocking // 0' <<<"$classified_record" 2>/dev/null || echo 0)"
total="$(jq -r '(.findings | length) // 0' <<<"$classified_record" 2>/dev/null || echo 0)"
case "$blocking" in
  ''|*[!0-9]*) blocking=1 ;;  # an unreadable count is not zero findings
esac
echo "consume-peer-verdict: classified $total finding(s): $blocking blocking, $nonblocking non-blocking"

# A finding line must carry CONTENT after the severity marker. The bare-marker
# case is a section header, not a finding: codex reliably emits `P1` / `P2` as
# empty headers above its findings, and counting those as findings rejected a
# genuinely clean review with "declares 0 blocking finding(s), but output
# contains 2". Nothing is hidden by this: a marker with no text after it states
# no defect, so it cannot be a finding someone is smuggling past the count.
observed="$(grep -Ec '^[[:space:]]*P[12][[:space:]:-]+[^[:space:]]' "$review_file" || true)"

[[ "$observed" -eq "$blocking" ]] || reject \
  "classified $blocking blocking finding(s), but output contains $observed"

case "$verdict:$blocking" in
  clean:0)
    emit pass "$payload_file"
    ;;
  blocked:0)
    reject "blocked verdict classifies zero blocking findings"
    ;;
  blocked:*)
    emit fail "$payload_file"
    echo "consume-peer-verdict: peer reported $blocking blocking finding(s)" >&2
    exit 1
    ;;
  clean:*)
    reject "clean verdict contradicts $blocking classified blocking finding(s)"
    ;;
esac
