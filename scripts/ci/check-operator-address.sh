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
    SCANNED=$((SCANNED + 1))
    if [ -n "$output" ]; then
        printf '%s\n' "$output"
        count="$(printf '%s\n' "$output" | wc -l | tr -d '[:space:]')"
        HITS=$((HITS + count))
    fi
}

scan_path() {
    local path="$1"
    if [ -d "$path" ]; then
        local files
        files="$(git ls-files -- "$path/*.md" "$path/**/*.md")" ||
            fail "could not enumerate markdown under $path"
        [ -n "$files" ] || fail "no tracked markdown files under $path"
        while IFS= read -r file; do
            [ -n "$file" ] || continue
            scan_and_count "$file"
        done <<< "$files"
    else
        scan_and_count "$path"
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
SCANNED=0

HITS=0
if [ "$#" -gt 0 ]; then
    for path in "$@"; do
        scan_path "$path"
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

if [ "$#" -gt 0 ]; then
    echo "check-operator-address: clean ($SCANNED tracked markdown files from requested paths)"
else
    echo "check-operator-address: clean ($SCANNED tracked skill markdown files)"
fi
