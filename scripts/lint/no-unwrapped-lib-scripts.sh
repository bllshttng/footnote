#!/usr/bin/env bash
# scripts/lint/no-unwrapped-lib-scripts.sh
#
# Lists every script in scripts/lib/*.sh and scripts/lib/*.py.
# For each script not in the allowlist, greps cli/src/fno/**/*.py
# for the script's basename. Missing = hard fail (exit 1).
#
# Exit codes:
#   0  all scripts/lib/ entries have wrappers (or are in the allowlist)
#   1  one or more entries are missing a wrapper; see stderr remediation
#
# Stdout format:
#   no-unwrapped-lib-scripts: checked N scripts, K missing wrappers
#
# Stderr format (per missing entry):
#   ERROR: scripts/lib/<name> has no fno wrapper. Add a wrapper in
#   cli/src/fno/ or add the script to
#   scripts/lint/.unwrapped-lib-allowlist.txt if it is a pure library.
#
# Bash 3.2 compatible.

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo "no-unwrapped-lib-scripts: error: git not found; run from inside the repo" >&2
    exit 0
}

ALLOWLIST_FILE="$REPO_ROOT/scripts/lint/.unwrapped-lib-allowlist.txt"
LIB_DIR="$REPO_ROOT/scripts/lib"
CLI_SRC="$REPO_ROOT/cli/src/fno"

# Build allowlist from the file (skip blank lines and comments).
_in_allowlist() {
    local name="$1"
    while IFS= read -r line; do
        [[ -z "$line" || "${line:0:1}" == "#" ]] && continue
        [[ "$line" == "$name" ]] && return 0
    done < "$ALLOWLIST_FILE"
    return 1
}

checked=0
missing=0
missing_names=()

for script_path in "$LIB_DIR"/*.sh "$LIB_DIR"/*.py; do
    [[ -e "$script_path" ]] || continue
    basename_script="$(basename "$script_path")"

    # Skip allowlisted entries.
    if _in_allowlist "$basename_script"; then
        continue
    fi

    checked=$((checked + 1))

    if ! grep -rq "$basename_script" "$CLI_SRC" --include='*.py' 2>/dev/null; then
        missing=$((missing + 1))
        missing_names+=("$basename_script")
    fi
done

echo "no-unwrapped-lib-scripts: checked $checked scripts, $missing missing wrappers"
for name in "${missing_names[@]+"${missing_names[@]}"}"; do
    {
        echo "ERROR: scripts/lib/$name has no fno wrapper."
        echo "  Add a wrapper in cli/src/fno/ or add the script to"
        echo "  scripts/lint/.unwrapped-lib-allowlist.txt if it is a pure library."
    } >&2
done

# Hard fail if any wrappers are missing (per PR 2 contract).
if [[ "$missing" -gt 0 ]]; then
    exit 1
fi
exit 0
