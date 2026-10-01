#!/usr/bin/env bash
# list-reviewers.sh - Emit configured external reviewers as TAB-separated
# "type<TAB>bot_login" lines, one per line.
#
# Config schema (in .fno/settings.yaml or ~/.fno/settings.yaml):
#
#   config:
#     # PREFERRED: list form. Each item is one of:
#     #   gemini | coderabbit | claude | codex
#     external_reviewers:
#       - gemini
#       - codex
#
#   # LEGACY: scalar form. Still supported, treated as a single-item list.
#   # If both are set, external_reviewers (list) wins.
#     external_reviewer: gemini
#
# Resolution order:
#   1. Local .fno/settings.yaml external_reviewers list (if non-empty)
#   2. Global ~/.fno/settings.yaml external_reviewers list (if non-empty)
#   3. Legacy scalar external_reviewer (local-over-global, via get_config)
#   4. Default: gemini
#
# "none" anywhere in the list disables external review for that entry.
# When the resulting list is empty (or all "none"), exits 0 with no output;
# callers should treat that as "external review disabled".

set -euo pipefail

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/config.sh"

LOCAL_SETTINGS_FILE="${LOCAL_SETTINGS:-.fno/config.toml}"
GLOBAL_SETTINGS_FILE="${GLOBAL_SETTINGS:-$HOME/.fno/config.toml}"

declare -a TYPES=()

# Read external_reviewers list. Tries yq first (handles both block-list and
# inline-list YAML forms); falls back to a pure-awk reader when yq is missing
# so the list config still works on fresh shells without `yq` installed.
# Local fully replaces global (no merge); merge semantics make it impossible
# to actually OVERRIDE the inherited list, only extend it.
_read_list_via_yq() {
    # Emit nothing (rc=0) when the key isn't present, instead of returning 1.
    # The `|| return 1` form interacted poorly with `set -e` callers in the
    # `list_output=$(...)` command-substitution context.
    #
    # Canonical path is config.review.external_reviewers; the legacy
    # config.external_reviewers is read only as a fallback.
    local file="$1"
    if yq -p toml -e '.review.external_reviewers' "$file" >/dev/null 2>&1; then
        yq -p toml -r '.review.external_reviewers[]?' "$file" 2>/dev/null
        return 0
    fi
    if yq -p toml -e '.external_reviewers' "$file" >/dev/null 2>&1; then
        yq -p toml -r '.external_reviewers[]?' "$file" 2>/dev/null
    fi
}

# Pure-awk fallback for flat config.toml (when yq is missing). Reads the
# `external_reviewers = [ ... ]` array (under a [review] table or top-level),
# both single-line inline and multi-line forms:
#     external_reviewers = ["gemini", "codex"]
#     external_reviewers = [
#       "gemini",
#       "codex",
#     ]
# Only the first such array is read (matching yq). Nested brackets / escaped
# commas are out of scope; those rare forms need yq.
_read_list_via_awk() {
    local file="$1"
    [[ -f "$file" ]] || return 1
    awk '
        collecting == 0 && /external_reviewers[[:space:]]*=[[:space:]]*\[/ {
            collecting = 1
        }
        collecting == 1 {
            acc = acc $0 "\n"
            if ($0 ~ /\]/) {
                sub(/^.*external_reviewers[[:space:]]*=[[:space:]]*\[/, "", acc)
                sub(/\][^]]*$/, "", acc)
                n = split(acc, items, ",")
                for (i = 1; i <= n; i++) {
                    v = items[i]
                    sub(/#.*/, "", v)
                    gsub(/[[:space:]]/, "", v)
                    gsub(/["'\'']/, "", v)
                    if (v != "") print v
                }
                collecting = 0
                acc = ""
            }
        }
    ' "$file"
}

for f in "$LOCAL_SETTINGS_FILE" "$GLOBAL_SETTINGS_FILE"; do
    [[ -f "$f" ]] || continue
    if command -v yq &>/dev/null; then
        list_output=$(_read_list_via_yq "$f")
    else
        list_output=$(_read_list_via_awk "$f")
    fi
    if [[ -n "$list_output" ]]; then
        while IFS= read -r line; do
            [[ -n "$line" && "$line" != "null" ]] && TYPES+=("$line")
        done <<< "$list_output"
        # First settings file with a non-empty list wins
        [[ ${#TYPES[@]} -gt 0 ]] && break
    fi
done

# Fall back to the scalar external_reviewer (uses get_config's local-over-global)
if [[ ${#TYPES[@]} -eq 0 ]]; then
    scalar=$(get_config "external_reviewer" "")
    [[ -n "$scalar" ]] && TYPES=("$scalar")
fi

# Default if nothing configured
[[ ${#TYPES[@]} -eq 0 ]] && TYPES=("gemini")

# Optional explicit override for the bot login (legacy single-reviewer config)
EXPLICIT_BOT=$(get_config "external_reviewer_bot" "")

# De-duplicate while preserving first-seen order. macOS bash 3.2 has no
# associative arrays, so we use a delimited accumulator string instead.
SEEN_STR=""
declare -a UNIQ=()
for t in "${TYPES[@]}"; do
    case "|${SEEN_STR}|" in
        *"|${t}|"*) continue ;;
    esac
    SEEN_STR="${SEEN_STR}|${t}"
    UNIQ+=("$t")
done

for t in "${UNIQ[@]}"; do
    case "$t" in
        none) continue ;;
        gemini)     bot="gemini-code-assist[bot]" ;;
        coderabbit) bot="coderabbitai[bot]" ;;
        claude)     bot="claude[bot]" ;;
        codex)      bot="chatgpt-codex-connector[bot]" ;;
        *)
            # Unknown type: use explicit external_reviewer_bot if provided,
            # otherwise warn and skip.
            if [[ -n "$EXPLICIT_BOT" ]]; then
                bot="$EXPLICIT_BOT"
            else
                echo "list-reviewers: unknown reviewer type '$t'; set config.external_reviewer_bot or use a known type (gemini|coderabbit|claude|codex)" >&2
                continue
            fi
            ;;
    esac
    # When exactly one reviewer is configured and the legacy bot override is
    # set, prefer the override. This keeps the historical single-reviewer
    # config path working unchanged.
    if [[ ${#UNIQ[@]} -eq 1 && -n "$EXPLICIT_BOT" ]]; then
        bot="$EXPLICIT_BOT"
    fi
    printf '%s\t%s\n' "$t" "$bot"
done
