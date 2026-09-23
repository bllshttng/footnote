#!/usr/bin/env bash
# Keep the human address in skills as "user".

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
CANARY="scripts/ci/fixtures/operator-address-canary.md"
cd "$REPO_ROOT"

fail() {
    echo "check-operator-address: $*" >&2
    exit 1
}

scan_file() {
    local path="$1"
    [ -f "$path" ] || fail "file not found: $path"
    awk -v file="$path" '
        BEGIN { fenced = 0 }
        {
            raw = $0
            if (raw ~ /^[[:space:]]*```/) {
                fenced = !fenced
                next
            }
            if (fenced) {
                next
            }
            text = tolower(raw)
            gsub(/`[^`]*`/, "", text)
            gsub(/operator[ -]ruling/, "", text)
            gsub(/operator[ -]override/, "", text)
            if (text ~ /(^|[^[:alnum:]_-])operator([^[:alnum:]_-]|$)/) {
                printf "%s:%d: %s\n", file, NR, raw
            }
        }
    ' "$path"
}

scan_and_count() {
    local path="$1"
    local output count
    output="$(scan_file "$path")" || fail "could not scan $path"
    if [ -n "$output" ]; then
        printf '%s\n' "$output"
        count="$(printf '%s\n' "$output" | wc -l | tr -d '[:space:]')"
        HITS=$((HITS + count))
    fi
}

[ -f "$CANARY" ] || fail "canary fixture not found at $CANARY"
canary_output="$(scan_file "$CANARY")" || fail "could not scan canary fixture"
case "$canary_output" in
    *CANARY-HIT*) ;;
    *) fail "tool control did not find CANARY-HIT" ;;
esac
canary_lines="$(printf '%s\n' "$canary_output" | wc -l | tr -d '[:space:]')"
[ "$canary_lines" -eq 1 ] || fail "tool control expected one canary hit, got $canary_lines"

default_files="$(git ls-files -- 'skills/*.md' 'skills/**/*.md')" ||
    fail "could not enumerate tracked skill markdown"
[ -n "$default_files" ] || fail "default skills markdown surface is empty"
surface_count="$(printf '%s\n' "$default_files" | wc -l | tr -d '[:space:]')"

HITS=0
if [ "$#" -gt 0 ]; then
    for path in "$@"; do
        scan_and_count "$path"
    done
else
    while IFS= read -r path; do
        [ -n "$path" ] || continue
        scan_and_count "$path"
    done <<< "$default_files"
fi

if [ "$HITS" -gt 0 ]; then
    echo "check-operator-address: operator is never the address (docs/architecture/vocabulary-user-and-operator.md). Write user for the human, superuser for the authority, or put a wire value in backticks." >&2
    exit 1
fi

echo "check-operator-address: clean ($surface_count tracked skill markdown files)"
