#!/usr/bin/env bash
# scripts/lint/no-invalid-events.sh
#
# Fails CI when any events.invalid.jsonl is non-empty. Producers route
# rejected events to .invalid.jsonl when running with --soft validation
# (hooks); this scanner ensures those rows do not silently accumulate
# across PRs without operator review.
#
# Walks:
#   .fno/events.invalid.jsonl
#   cli/.fno/events.invalid.jsonl
#   .fno/artifacts/events.invalid.jsonl
#   .claude/worktrees/*/.fno/events.invalid.jsonl
#
# Exit codes:
#   0  no quarantined events anywhere
#   1  one or more non-empty quarantine files (each printed to stderr
#      with file path, row count, and a one-line remediation)
#   2  substrate failure (git unavailable)

set -uo pipefail

if ! REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"; then
    echo "lint script unavailable: missing dependency: git" >&2
    exit 2
fi

cd "$REPO_ROOT" || {
    echo "lint script unavailable: cannot cd to repo root" >&2
    exit 2
}

violations=0

_check_path() {
    local path="$1"
    if [[ -s "$path" ]]; then
        local n
        n=$(wc -l < "$path" | tr -d ' ')
        echo "$path: $n quarantined events; review and clear before merge" >&2
        echo "  remediation: inspect rows, fix the producer, then truncate the file with ': > $path'" >&2
        violations=$((violations + 1))
    fi
}

# Known canonical paths.
for path in \
    .fno/events.invalid.jsonl \
    cli/.fno/events.invalid.jsonl \
    .fno/artifacts/events.invalid.jsonl
do
    _check_path "$path"
done

# Worktree-namespace paths.
if [[ -d .claude/worktrees ]]; then
    while IFS= read -r p; do
        [[ -z "$p" ]] && continue
        _check_path "$p"
    done < <(find .claude/worktrees -path '*/.fno/events.invalid.jsonl' 2>/dev/null || true)
fi

if [[ $violations -gt 0 ]]; then
    exit 1
fi
exit 0
