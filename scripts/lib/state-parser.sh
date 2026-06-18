#!/usr/bin/env bash
# state-parser.sh - shared field-extraction helpers for target-state.md
# and megawalk-state.md.
#
# Centralizes the `grep | head -1 | sed | tr -d` pattern that previously
# appeared in target-stop-hook.sh, megawalk-stop-hook.sh, and the various
# emit-helper scripts. A single helper gives us one place to update parsing
# logic when the state file format evolves (block-style YAML support,
# multi-line values, etc.).

# read_state_field STATE_FILE FIELD
#   Extract a top-level YAML scalar field from a markdown frontmatter file.
#   Strips leading/trailing whitespace AND surrounding single or double
#   quotes. Returns empty string when the field is missing, the file is
#   missing, or the value is the literal string `null`.
#
#   stdout: the value (possibly empty)
#   rc:     always 0 (callers check for empty string, not exit code)
#
# This intentionally does NOT support block-style YAML values (`field: |`)
# or nested mappings. Those are documented as unsupported in
# skills/target/references/gate-artifacts.md.
read_state_field() {
    local file="${1:?file required}"
    local key="${2:?key required}"
    [[ -f "$file" ]] || { echo ""; return 0; }
    local value
    value=$(grep -E "^${key}:" "$file" 2>/dev/null \
        | head -1 \
        | sed -e "s/^${key}:[[:space:]]*//" -e 's/[[:space:]]*$//' \
        | tr -d '"' | tr -d "'")
    if [[ "$value" == "null" ]]; then
        echo ""
    else
        echo "$value"
    fi
}
