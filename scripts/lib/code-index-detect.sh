#!/usr/bin/env bash
# code-index-detect.sh - which code-index providers are present in a checkout.
#
# Prints one tab-separated line per present provider:
#   name <TAB> roles (comma-joined) <TAB> ready | unavailable:<reason> <TAB> manifest path
#
# A provider is one TOML file (docs/code-index-providers.md). Only single-line
# `key = "value"` and `roles = [...]` lines are read. A malformed manifest is
# skipped with one stderr line and never blocks the others. The script always
# exits 0: a broken provider is an absent provider. It never runs `ask`,
# `fresh` or `refresh`.
#
# Search order, later wins by name: the bundled code-index/providers/ next to
# this script (deployed skill layout, then repo layout),
# ~/.fno/code-index/providers/, <repo>/.fno/code-index/providers/. The fourth
# column is the manifest's canonical absolute path: every directory is
# resolved before the glob, so no printed path carries a . or .. segment.
# A valid name shadows lower-priority manifests before its index presence is
# checked: an override whose index is absent never falls back to the bundled
# provider of the same name.
#
# Usage: code-index-detect.sh [repo-root]   (default: git toplevel, then $PWD)
# Bash 3.2 compatible.

set -u

REPO_ROOT="${1:-}"
if [[ -z "$REPO_ROOT" ]]; then
    REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || true)"
    [[ -n "$REPO_ROOT" ]] || REPO_ROOT="$PWD"
fi
# A caller may pass a relative or ..-carrying root; the repo override leg
# prints what it is given, so resolve it here to keep every path canonical.
[[ -d "$REPO_ROOT" ]] && REPO_ROOT="$(cd "$REPO_ROOT" && pwd)"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

skip() { echo "code-index: skipped $1: $2" >&2; }

manifest_name() {
    sed -n 's/^[[:space:]]*name[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p' "$1" | head -1
}

# The name is already validated and shadow-marked by the caller: a present
# name shadows lower-priority manifests BEFORE presence is checked, so an
# override whose index is absent never falls back to the bundled one.
provider_line() {
    local file="$1" name="$2"
    local detect roles requires status
    detect=$(sed -n 's/^[[:space:]]*detect[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p' "$file" | head -1)
    if [[ -z "$detect" ]]; then
        skip "$file" "no detect line"
        return 0
    fi
    roles=$(sed -n 's/^[[:space:]]*roles[[:space:]]*=[[:space:]]*\[\(.*\)\][[:space:]]*$/\1/p' "$file" | head -1 | tr -d '" ' )
    requires=$(sed -n 's/^[[:space:]]*requires[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p' "$file" | head -1)
    [[ -e "$REPO_ROOT/$detect" ]] || return 0
    status="ready"
    if [[ -n "$requires" ]] && ! command -v "$requires" >/dev/null 2>&1; then
        status="unavailable:requires $requires not on PATH"
    fi
    printf '%s\t%s\t%s\t%s\n' "$name" "$roles" "$status" "$file"
}

PROVIDER_FILES=()
add_dir() {
    local dir="$1" f
    [[ -d "$dir" ]] || return 0
    # Logical pwd, never -P: a /var mktemp path must keep its spelling.
    dir="$(cd "$dir" && pwd)"
    for f in "$dir"/*.toml; do
        [[ -f "$f" ]] || continue
        PROVIDER_FILES+=("$f")
    done
}
add_dir "$SCRIPT_DIR/../../code-index/providers"
add_dir "$SCRIPT_DIR/../../skills/blueprint/code-index/providers"
add_dir "${HOME:-}/.fno/code-index/providers"
add_dir "$REPO_ROOT/.fno/code-index/providers"

seen=""
i=${#PROVIDER_FILES[@]}
while (( i > 0 )); do
    i=$((i-1))
    f="${PROVIDER_FILES[$i]}"
    name=$(manifest_name "$f")
    if [[ -z "$name" ]]; then
        skip "$f" "no name line"
        continue
    fi
    if [[ ! "$name" =~ ^[a-z0-9][a-z0-9-]*$ ]]; then
        skip "$f" "bad name '$name'"
        continue
    fi
    if [[ :$seen != *":$name:"* ]]; then
        seen="$seen:$name:"
        line=$(provider_line "$f" "$name")
        [[ -n "$line" ]] && printf '%s\n' "$line"
    fi
done

exit 0
