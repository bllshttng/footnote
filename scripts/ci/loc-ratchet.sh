#!/usr/bin/env bash
# scripts/ci/loc-ratchet.sh
#
# LOC gate for the control-plane scope: counts the line-count delta (raw git
# diff --numstat lines, NOT executable-LOC - comments and blanks are included)
# for every PR inside a checked-in path manifest and fails when the delta is
# positive, unless a `loc-exception:` line is present in the PR body.
#
# NOTE: this is a per-PR delta gate, not a ratchet against a baseline. There is
# no backsliding-over-time mechanism; an earlier CUMULATIVE metric and a checked
# in trajectory log were removed because nothing consumed them and every
# loc-exception PR conflicted on the append-only log by construction.
#
# Usage:
#   CI mode:   loc-ratchet.sh
#              BASE_REF env (from github.base_ref) drives "origin/$BASE_REF"
#   Local run: loc-ratchet.sh --base <ref>
#              <ref> can be any git ref (branch, tag, hash, origin/main)
#
# Exit codes:
#   0  delta <= 0 (PASS), or delta > 0 with a `loc-exception:` line in PR_BODY
#   1  delta > 0 with no `loc-exception:` line, or error
#
# Environment variables (all optional except BASE_REF in CI):
#   BASE_REF             - set by GitHub Actions from github.base_ref
#   LOC_RATCHET_MANIFEST - override manifest path (tests-only; not for CI use)
#   PR_BODY              - the PR body (exception source; set by CI from the PR)
#   GITHUB_STEP_SUMMARY  - if set, append markdown summary there (set by GHA)
#
# Portability: requires bash 3.2+, git, awk (POSIX), grep.
# Forbidden: timeout, stat -f, stat -c, mapfile, grep -P, GNU-only sed flags.

set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────────
# Configuration
# ─────────────────────────────────────────────────────────────────────────────

REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null) \
    || { echo "ERROR: not in a git repository" >&2; exit 1; }

MANIFEST="${LOC_RATCHET_MANIFEST:-${REPO_ROOT}/scripts/ci/loc-ratchet-manifest.yaml}"

# ─────────────────────────────────────────────────────────────────────────────
# Argument parsing
# ─────────────────────────────────────────────────────────────────────────────

BASE_OVERRIDE=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --base)
            [[ $# -ge 2 ]] || { echo "ERROR: --base requires an argument" >&2; exit 1; }
            BASE_OVERRIDE="$2"
            shift 2
            ;;
        *)
            echo "ERROR: unknown argument: $1" >&2
            echo "Usage: loc-ratchet.sh [--base <ref>]" >&2
            exit 1
            ;;
    esac
done

# Determine BASE ref
if [[ -n "$BASE_OVERRIDE" ]]; then
    BASE="$BASE_OVERRIDE"
elif [[ -n "${BASE_REF:-}" ]]; then
    BASE="origin/${BASE_REF}"
else
    echo "ERROR: no base ref; set BASE_REF env or pass --base <ref>" >&2
    exit 1
fi

# ─────────────────────────────────────────────────────────────────────────────
# Manifest parsing (line-oriented YAML subset, awk/grep only - locked decision 10)
# ─────────────────────────────────────────────────────────────────────────────

[[ -f "$MANIFEST" ]] \
    || { echo "ERROR: manifest not found: $MANIFEST" >&2; exit 1; }

# Parse include entries (lines under `include:` section, before next top-level key)
# Output: one entry per line, stripped of leading '  - ' and surrounding quotes
parse_section() {
    local file="$1" section="$2"
    awk -v sec="$section" '
        /^[a-z]/ { in_sec = ($0 == sec ":") }
        in_sec && /^  - / {
            val = $0
            sub(/^  - /, "", val)
            # strip surrounding single or double quotes
            if (substr(val,1,1) == "\"" || substr(val,1,1) == "'"'"'") {
                val = substr(val, 2, length(val)-2)
            }
            print val
        }
    ' "$file"
}

# Read sections into variables (bash 3.2 compatible: no mapfile, use while+read).
# Capture parse_section output to a variable with explicit rc check (F1: observe
# the underlying command's exit code rather than letting process substitution
# swallow it).
INCLUDE_RAW=$(parse_section "$MANIFEST" "include") \
    || { echo "ERROR: failed to parse include: section from manifest: $MANIFEST" >&2; exit 1; }
INCLUDE_ENTRIES=""
while IFS= read -r line; do
    INCLUDE_ENTRIES="${INCLUDE_ENTRIES}${line}"$'\n'
done <<< "$INCLUDE_RAW"

EXTENSION_RAW=$(parse_section "$MANIFEST" "extensions") \
    || { echo "ERROR: failed to parse extensions: section from manifest: $MANIFEST" >&2; exit 1; }
EXTENSION_ENTRIES=""
while IFS= read -r line; do
    EXTENSION_ENTRIES="${EXTENSION_ENTRIES}${line}"$'\n'
done <<< "$EXTENSION_RAW"

EXCLUDE_RAW=$(parse_section "$MANIFEST" "exclude") \
    || { echo "ERROR: failed to parse exclude: section from manifest: $MANIFEST" >&2; exit 1; }
EXCLUDE_ENTRIES=""
while IFS= read -r line; do
    EXCLUDE_ENTRIES="${EXCLUDE_ENTRIES}${line}"$'\n'
done <<< "$EXCLUDE_RAW"

# Helper: strip all whitespace (spaces + newlines) from a variable for emptiness check.
# "// /}" only strips spaces; we also need to strip newlines (from <<< feeding).
_strip_ws() { printf '%s' "$1" | tr -d ' \t\n\r'; }

# Validate: include must not be empty
if [[ -z "$(_strip_ws "$INCLUDE_ENTRIES")" ]]; then
    echo "ERROR: manifest has empty or missing include: section: $MANIFEST" >&2
    exit 1
fi

# Validate: extensions must not be empty (F2: empty extensions = nothing matches =
# delta always 0 = false PASS; fail closed).
if [[ -z "$(_strip_ws "$EXTENSION_ENTRIES")" ]]; then
    echo "ERROR: manifest has empty or missing extensions: section: $MANIFEST" >&2
    echo "  An empty extensions list means no files would ever match, producing a false PASS." >&2
    echo "  Add at least one extension (e.g. sh, py, yaml, rs) to the extensions: section." >&2
    exit 1
fi

# ─────────────────────────────────────────────────────────────────────────────
# Merge-base computation
# ─────────────────────────────────────────────────────────────────────────────

MB=$(git merge-base "$BASE" HEAD 2>/dev/null) \
    || { echo "ERROR: cannot compute merge-base between '$BASE' and HEAD" >&2
         echo "  This usually means a shallow clone or unreachable base ref." >&2
         echo "  In GitHub Actions ensure fetch-depth: 0 in actions/checkout." >&2
         echo "  For local runs, ensure '$BASE' is reachable (try: git fetch origin)." >&2
         exit 1; }

# ─────────────────────────────────────────────────────────────────────────────
# File matcher: returns 0 (match) or 1 (no match)
# Applies include-prefix/glob, extension whitelist, and exclude patterns.
# ─────────────────────────────────────────────────────────────────────────────

# file_matches <filepath>
# Returns 0 if the file should be counted, 1 otherwise.
file_matches() {
    local filepath="$1"
    local basename ext

    # Get basename (portable: no basename command dependency for safety)
    basename="${filepath##*/}"

    # Get extension (part after last dot; empty if no dot)
    if [[ "$basename" == *.* ]]; then
        ext="${basename##*.}"
    else
        ext=""
    fi

    # 1. Extension whitelist check
    local ext_ok=0
    while IFS= read -r allowed_ext; do
        allowed_ext="${allowed_ext#"${allowed_ext%%[![:space:]]*}"}"
        allowed_ext="${allowed_ext%"${allowed_ext##*[![:space:]]}"}"
        [[ -z "$allowed_ext" ]] && continue
        if [[ "$ext" == "$allowed_ext" ]]; then
            ext_ok=1
            break
        fi
    done <<< "$EXTENSION_ENTRIES"
    [[ "$ext_ok" -eq 1 ]] || return 1

    # 2. Include check: at least one include entry must match
    local include_ok=0
    while IFS= read -r entry; do
        entry="${entry#"${entry%%[![:space:]]*}"}"
        entry="${entry%"${entry##*[![:space:]]}"}"
        [[ -z "$entry" ]] && continue

        if [[ "$entry" == */ ]]; then
            # Directory prefix: filepath starts with entry
            if [[ "$filepath" == "${entry}"* ]]; then
                include_ok=1
                break
            fi
        elif [[ "$entry" == *\* ]]; then
            # Path-prefix glob: filepath starts with the part before *
            local prefix="${entry%\*}"
            if [[ "$filepath" == "${prefix}"* ]]; then
                include_ok=1
                break
            fi
        else
            # Exact file match
            if [[ "$filepath" == "$entry" ]]; then
                include_ok=1
                break
            fi
        fi
    done <<< "$INCLUDE_ENTRIES"
    [[ "$include_ok" -eq 1 ]] || return 1

    # 3. Exclude check: if any exclude pattern matches, exclude the file
    while IFS= read -r pattern; do
        pattern="${pattern#"${pattern%%[![:space:]]*}"}"
        pattern="${pattern%"${pattern##*[![:space:]]}"}"
        [[ -z "$pattern" ]] && continue

        # Strip leading **/ from pattern
        local stripped_pattern="${pattern#\*\*/}"

        if [[ "$stripped_pattern" == *"/**" ]]; then
            # Path-segment rule: path contains the directory segment
            local dir_seg="${stripped_pattern%/**}"
            # Match if filepath starts with dir_seg/ or contains /dir_seg/
            if [[ "$filepath" == "${dir_seg}/"* ]] || \
               [[ "$filepath" == *"/${dir_seg}/"* ]]; then
                return 1
            fi
        else
            # Basename rule: match basename against the glob pattern.
            # shellcheck disable=SC2254  # intentional: unquoted so the
            # exclude pattern (test_*, *_test.*) glob-matches, not literal.
            case "$basename" in
                $stripped_pattern) return 1 ;;
            esac
        fi
    done <<< "$EXCLUDE_ENTRIES"

    return 0
}

# ─────────────────────────────────────────────────────────────────────────────
# Delta computation: git diff --numstat --no-renames from merge-base to HEAD
# ─────────────────────────────────────────────────────────────────────────────

# Collect per-file diff stats for matched files.
# F1: capture git diff output to a variable with explicit rc check so a partial
# diff (e.g. git error mid-stream) never produces a false partial sum.
DIFF_OUT=$(git diff --numstat --no-renames "$MB" HEAD) \
    || { echo "ERROR: git diff failed (exit $?); cannot compute delta" >&2; exit 1; }

# This measures COMMITTED HEAD. Run locally with the change still staged or
# unstaged and the gate honestly reports the delta of a tree that does not
# contain it -- a "PASS: delta <= 0" that CI then contradicts. Warn rather than
# fail: a dirty tree is legitimate, reading the verdict as final is not.
if ! git diff --quiet HEAD 2>/dev/null || ! git diff --cached --quiet 2>/dev/null; then
    echo "WARNING: working tree is dirty; this gate measures COMMITTED HEAD only." >&2
    echo "         Commit first, or this delta excludes your uncommitted changes." >&2
fi

MATCHED_FILES=""
TOTAL_ADDED=0
TOTAL_DELETED=0

while IFS=$'\t' read -r added deleted filepath; do
    # Skip binary rows (numstat emits "-" for binary files)
    if [[ "$added" == "-" ]] || [[ "$deleted" == "-" ]]; then
        continue
    fi
    # Validate numeric
    if ! [[ "$added" =~ ^[0-9]+$ ]] || ! [[ "$deleted" =~ ^[0-9]+$ ]]; then
        continue
    fi

    # Apply manifest filter
    if file_matches "$filepath"; then
        file_delta=$((added - deleted))
        MATCHED_FILES="${MATCHED_FILES}${filepath}"$'\t'"${added}"$'\t'"${deleted}"$'\t'"${file_delta}"$'\n'
        TOTAL_ADDED=$((TOTAL_ADDED + added))
        TOTAL_DELETED=$((TOTAL_DELETED + deleted))
    fi
done <<< "$DIFF_OUT"

DELTA=$((TOTAL_ADDED - TOTAL_DELETED))

# ─────────────────────────────────────────────────────────────────────────────
# Output: per-file breakdown and delta
# ─────────────────────────────────────────────────────────────────────────────

echo ""
echo "=== LOC Ratchet ==="
echo ""

if [[ -n "$MATCHED_FILES" ]]; then
    echo "Per-file breakdown (manifest-matched changes):"
    printf '  %-60s %6s %6s %6s\n' "File" "+Added" "-Deleted" "Delta"
    printf '  %-60s %6s %6s %6s\n' \
        "------------------------------------------------------------" \
        "------" "--------" "-----"
    while IFS=$'\t' read -r filepath added deleted file_delta; do
        [[ -z "$filepath" ]] && continue
        printf '  %-60s %6s %6s %6s\n' "$filepath" "$added" "$deleted" "$file_delta"
    done <<< "$MATCHED_FILES"
    echo ""
fi

if [[ "$DELTA" -gt 0 ]]; then
    echo "  delta: +${DELTA} (POSITIVE - gate triggered)"
elif [[ "$DELTA" -lt 0 ]]; then
    echo "  delta: ${DELTA} (negative - LOC reduced)"
else
    echo "  delta: 0 (no change)"
fi

echo ""

# GitHub step summary (if running in GitHub Actions)
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    {
        echo "## LOC Ratchet Summary"
        echo ""
        if [[ -n "$MATCHED_FILES" ]]; then
            echo "| File | +Added | -Deleted | Delta |"
            echo "|------|--------|----------|-------|"
            while IFS=$'\t' read -r filepath added deleted file_delta; do
                [[ -z "$filepath" ]] && continue
                echo "| \`${filepath}\` | +${added} | -${deleted} | ${file_delta} |"
            done <<< "$MATCHED_FILES"
            echo ""
        fi
        echo "- **Delta**: ${DELTA}"
    } >> "$GITHUB_STEP_SUMMARY"
fi

# ─────────────────────────────────────────────────────────────────────────────
# Gate decision
# ─────────────────────────────────────────────────────────────────────────────

if [[ "$DELTA" -le 0 ]]; then
    echo "PASS: delta <= 0."
    exit 0
fi

# delta > 0: the sole exception is a `loc-exception:` line in the PR body with a
# non-empty rationale. Read PR_BODY from env only (never eval or interpolate it).
PR_BODY_VALUE="${PR_BODY:-}"
BODY_FACTOR=0
if [[ -n "$PR_BODY_VALUE" ]]; then
    while IFS= read -r bodyline; do
        if printf '%s\n' "$bodyline" | grep -qE '^loc-exception:[[:space:]]*[^[:space:]]'; then
            BODY_FACTOR=1
            break
        fi
    done <<< "$PR_BODY_VALUE"
fi

if [[ "$BODY_FACTOR" -eq 1 ]]; then
    echo "PASS (exception declared): delta=+${DELTA}"
    if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
        printf '::warning title=LOC exception::delta=+%s\n' "${DELTA}"
    fi
    exit 0
fi

# No exception declared.
{
    echo "FAIL: delta > 0 (+${DELTA} lines added to control-plane scope)." >&2
    echo "" >&2
    if [[ -n "$MATCHED_FILES" ]]; then
        echo "Per-file breakdown:" >&2
        while IFS=$'\t' read -r filepath added deleted file_delta; do
            [[ -z "$filepath" ]] && continue
            printf '  %-60s %6s %6s %6s\n' "$filepath" "$added" "$deleted" "$file_delta" >&2
        done <<< "$MATCHED_FILES"
        echo "" >&2
    fi
    echo "To declare an exception, add this line to the PR body:" >&2
    echo "       loc-exception: <your rationale here>" >&2
} >&2
exit 1
