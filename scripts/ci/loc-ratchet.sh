#!/usr/bin/env bash
# scripts/ci/loc-ratchet.sh
#
# LOC ratchet CI gate for the control-plane collapse initiative (ab-c9777486).
#
# Counts executable-LOC delta for every PR inside a checked-in path manifest
# and fails when the delta is positive, unless the exception protocol is
# satisfied (Task 1.2). Task 1.1 implements the counting core only; the
# exception protocol is a stub that exits 1 with an explicit message.
#
# Usage:
#   CI mode:   loc-ratchet.sh
#              BASE_REF env (from github.base_ref) drives "origin/$BASE_REF"
#   Local run: loc-ratchet.sh --base <ref>
#              <ref> can be any git ref (branch, tag, hash, origin/main)
#
# Exit codes:
#   0  delta <= 0 (PASS)
#   1  delta > 0 (exception protocol not yet implemented) or error
#
# Environment variables (all optional except BASE_REF in CI):
#   BASE_REF               - set by GitHub Actions from github.base_ref
#   LOC_RATCHET_MANIFEST   - override manifest path (tests-only; not for CI use)
#   LOC_RATCHET_TRAJECTORY - override trajectory path (tests-only; not for CI use)
#   GITHUB_STEP_SUMMARY    - if set, append markdown summary there (set by GHA)
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
TRAJECTORY="${LOC_RATCHET_TRAJECTORY:-${REPO_ROOT}/scripts/ci/loc-ratchet-trajectory.yaml}"

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
# Trajectory parsing
# ─────────────────────────────────────────────────────────────────────────────

[[ -f "$TRAJECTORY" ]] \
    || { echo "ERROR: trajectory file not found: $TRAJECTORY" >&2
         echo "  Expected: $TRAJECTORY" >&2
         echo "  The trajectory file records the frozen baseline LOC count." >&2
         echo "  Task 1.3 creates it; see scripts/ci/loc-ratchet-trajectory.yaml" >&2
         exit 1; }

# Parse baseline.executable_loc (POSIX awk only - no match() with array capture)
BASELINE_LOC=$(grep 'executable_loc:' "$TRAJECTORY" | head -1 \
    | awk -F: '{gsub(/[[:space:]]/, "", $2); print $2}')

if [[ -z "$BASELINE_LOC" ]] || ! [[ "$BASELINE_LOC" =~ ^[0-9]+$ ]]; then
    echo "ERROR: trajectory file missing or unparseable baseline.executable_loc: $TRAJECTORY" >&2
    exit 1
fi

# Require the entries: key to be present (missing = structural parse failure, not
# "zero entries"). An empty list under entries: is valid; a missing key is not.
if ! grep -q '^entries:' "$TRAJECTORY"; then
    echo "ERROR: trajectory file is missing required 'entries:' key: $TRAJECTORY" >&2
    echo "  The entries: section records LOC exception history." >&2
    echo "  An empty entries: (no list items) is valid; the key itself must be present." >&2
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
# Live count: git ls-files at HEAD filtered by manifest, count via git cat-file
# ─────────────────────────────────────────────────────────────────────────────

LIVE_COUNT=0

# Collect matched live files into a temp list.
# F1: capture git ls-files output with explicit rc check.
LS_OUT=$(git ls-files) \
    || { echo "ERROR: git ls-files failed (exit $?); cannot compute live count" >&2; exit 1; }

LIVE_FILE_LIST=""
while IFS= read -r filepath; do
    if file_matches "$filepath"; then
        LIVE_FILE_LIST="${LIVE_FILE_LIST}${filepath}"$'\n'
    fi
done <<< "$LS_OUT"

if [[ -n "${LIVE_FILE_LIST// /}" ]]; then
    # Count lines per file using git cat-file to read blob content
    # (avoids stat portability issues; works on macOS bash 3.2 and ubuntu).
    # F5: a file listed by ls-files that fails to read = exit 1 (no silent skip).
    # For files absent at HEAD (first-exception edge case), use cat-file -e to
    # probe existence first (F4 pattern): absent = skip (documented edge);
    # present but unreadable = exit 1.
    while IFS= read -r filepath; do
        [[ -z "$filepath" ]] && continue
        # Probe existence at HEAD
        if ! git cat-file -e "HEAD:${filepath}" 2>/dev/null; then
            # File tracked by ls-files but absent at HEAD: genuinely deleted
            # (possible in a detached HEAD or unusual state); skip it.
            continue
        fi
        # awk END{NR} counts a final unterminated line (wc -l does not),
        # matching git's line model; baseline equivalence verified at switch.
        line_count=$(git cat-file blob "HEAD:${filepath}" | awk 'END {print NR}') \
            || { echo "ERROR: failed to read blob HEAD:${filepath}" >&2; exit 1; }
        LIVE_COUNT=$((LIVE_COUNT + line_count))
    done <<< "$LIVE_FILE_LIST"
fi

CUMULATIVE=$((LIVE_COUNT - BASELINE_LOC))

# ─────────────────────────────────────────────────────────────────────────────
# Output: per-file breakdown, delta, cumulative
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
    echo "  delta: +${DELTA} (POSITIVE - ratchet triggered)"
elif [[ "$DELTA" -lt 0 ]]; then
    echo "  delta: ${DELTA} (negative - LOC reduced)"
else
    echo "  delta: 0 (no change)"
fi

if [[ "$CUMULATIVE" -gt 0 ]]; then
    echo "  cumulative: +${CUMULATIVE} (live_count=${LIVE_COUNT} - baseline=${BASELINE_LOC})"
    echo "  WARNING: cumulative is above baseline; initiative still in debt"
elif [[ "$CUMULATIVE" -lt 0 ]]; then
    echo "  cumulative: ${CUMULATIVE} (live_count=${LIVE_COUNT} - baseline=${BASELINE_LOC})"
    echo "  Initiative is ahead of baseline (debt repaid)."
else
    echo "  cumulative: 0 (live_count=${LIVE_COUNT} - baseline=${BASELINE_LOC})"
    echo "  At baseline."
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
        echo "- **Cumulative** (live - baseline): ${CUMULATIVE} (live=${LIVE_COUNT}, baseline=${BASELINE_LOC})"
    } >> "$GITHUB_STEP_SUMMARY"
fi

# ─────────────────────────────────────────────────────────────────────────────
# Gate decision
# ─────────────────────────────────────────────────────────────────────────────

if [[ "$DELTA" -le 0 ]]; then
    echo "PASS: delta <= 0; ratchet satisfied."
    exit 0
fi

# ─────────────────────────────────────────────────────────────────────────────
# delta > 0: exception protocol (Task 1.2)
#
# Two factors required (both must hold to pass):
#   Factor 1 - PR body: env PR_BODY contains a line matching
#              ^loc-exception:[[:space:]]*[^[:space:]] (non-empty rationale)
#   Factor 2 - Trajectory: the diff MB..HEAD adds EXACTLY ONE new entry under
#              entries: in the trajectory file, with delta: == DELTA (exact)
#              and reason: non-empty.
#
# New-entry detection: parse entries at HEAD and at MB using `git show`.
# If the file does not exist at MB (first-ever exception PR), all HEAD entries
# count as new. This correctly fails when the seed file + a borrow land
# together (two new entries at once).
# ─────────────────────────────────────────────────────────────────────────────

# Self-remediating fail helper: prints per-file breakdown + remediation steps.
fail_exception() {
    local reason="$1"
    echo "FAIL: delta > 0 (+${DELTA} lines added to control-plane scope)." >&2
    echo "" >&2
    echo "Reason: ${reason}" >&2
    echo "" >&2
    if [[ -n "$MATCHED_FILES" ]]; then
        echo "Per-file breakdown:" >&2
        while IFS=$'\t' read -r filepath added deleted file_delta; do
            [[ -z "$filepath" ]] && continue
            printf '  %-60s %6s %6s %6s\n' "$filepath" "$added" "$deleted" "$file_delta" >&2
        done <<< "$MATCHED_FILES"
        echo "" >&2
    fi
    echo "To declare an exception, BOTH of the following are required:" >&2
    echo "" >&2
    echo "  1. Add this line to the PR body:" >&2
    echo "       loc-exception: <your rationale here>" >&2
    echo "" >&2
    echo "  2. Add EXACTLY ONE entry to scripts/ci/loc-ratchet-trajectory.yaml under entries::" >&2
    echo "       - date: $(date '+%Y-%m-%d')" >&2
    echo "         pr: <your PR number>" >&2
    echo "         branch: <your branch name>" >&2
    echo "         delta: ${DELTA}" >&2
    echo "         reason: \"<your rationale here>\"" >&2
    exit 1
}

# ── Factor 1: PR body check ───────────────────────────────────────────────────
# Read PR_BODY from env only (never eval or interpolate into commands).
PR_BODY_VALUE="${PR_BODY:-}"
BODY_FACTOR=0
if [[ -n "$PR_BODY_VALUE" ]]; then
    # Match any line that starts with loc-exception: followed by non-whitespace
    # Process line by line to avoid multi-line grep portability issues
    while IFS= read -r bodyline; do
        if printf '%s\n' "$bodyline" | grep -qE '^loc-exception:[[:space:]]*[^[:space:]]'; then
            BODY_FACTOR=1
            break
        fi
    done <<< "$PR_BODY_VALUE"
fi

# ── Factor 2: Trajectory new-entry check ────────────────────────────────────
# Relative path of trajectory (for git show)
TRAJ_REL=$(git ls-files --full-name -- "$TRAJECTORY" 2>/dev/null \
    || echo "")
if [[ -z "$TRAJ_REL" ]]; then
    # File may be newly added (not yet tracked) or path-overridden for tests.
    # Compute relative path from repo root.
    TRAJ_REL="${TRAJECTORY#${REPO_ROOT}/}"
fi

# parse_entries <yaml-content>
# Outputs one canonical line per entry.
# Format: date=<D><TAB>branch=<B><TAB>delta=<N><TAB>reason=<text>
# Identity for append-only checking is date+branch+delta+reason.
# pr: is intentionally EXCLUDED from identity (backfilling pr: null->N on an
# existing entry during review must not trip the append-only check).
# F3: uses tab as the canonical delimiter (cannot appear in a YAML scalar value
# that survived line-oriented parsing); also strips inline comments from delta.
# Parses line-oriented YAML entry blocks (- date: ... delta: ... reason: ...)
parse_entries() {
    awk '
    /^entries:/ { in_entries=1; next }
    in_entries && /^[a-z]/ && !/^  / { in_entries=0 }
    in_entries && /^  - / {
        # Start new entry block; flush previous entry
        if (delta != "" || reason != "") {
            print "date=" date "\tbranch=" branch "\tdelta=" delta "\treason=" reason
        }
        date=""
        branch=""
        delta=""
        reason=""
        # The list marker line may inline the date field: "  - date: 2026-06-04"
        if (match($0, /date:[[:space:]]*/)) {
            val=$0
            sub(/^.*date:[[:space:]]*/, "", val)
            sub(/[[:space:]]*$/, "", val)
            date=val
        }
        next
    }
    in_entries && /^    date:/ {
        val=$0
        sub(/^    date:[[:space:]]*/, "", val)
        sub(/[[:space:]]*$/, "", val)
        date=val
        next
    }
    in_entries && /^    branch:/ {
        val=$0
        sub(/^    branch:[[:space:]]*/, "", val)
        sub(/[[:space:]]*$/, "", val)
        branch=val
        next
    }
    in_entries && /^    delta:/ {
        val=$0
        sub(/^    delta:[[:space:]]*/, "", val)
        # strip inline comments (e.g. "42 # some note")
        sub(/[[:space:]]*#.*$/, "", val)
        # strip trailing whitespace
        sub(/[[:space:]]*$/, "", val)
        delta=val
        next
    }
    in_entries && /^    reason:/ {
        val=$0
        sub(/^    reason:[[:space:]]*/, "", val)
        # strip surrounding quotes
        if (substr(val,1,1) == "\"" || substr(val,1,1) == "'"'"'") {
            val = substr(val, 2, length(val)-2)
        }
        reason=val
        next
    }
    END {
        if (delta != "" || reason != "") {
            print "date=" date "\tbranch=" branch "\tdelta=" delta "\treason=" reason
        }
    }
    '
}

# F4: use cat-file -e to probe existence before piping through parse_entries.
# Genuinely absent (first-exception PR) = empty entries (documented edge).
# Present but unreadable or parse failure = exit 1.

# Get entries at HEAD
if git cat-file -e "HEAD:${TRAJ_REL}" 2>/dev/null; then
    HEAD_ENTRIES=$(git show "HEAD:${TRAJ_REL}" | parse_entries) \
        || { echo "ERROR: failed to parse trajectory entries at HEAD:${TRAJ_REL}" >&2; exit 1; }
else
    HEAD_ENTRIES=""
fi

# Get entries at MB (missing = first-ever exception PR; all HEAD entries are new)
if git cat-file -e "${MB}:${TRAJ_REL}" 2>/dev/null; then
    MB_ENTRIES=$(git show "${MB}:${TRAJ_REL}" | parse_entries) \
        || { echo "ERROR: failed to parse trajectory entries at ${MB}:${TRAJ_REL}" >&2; exit 1; }
else
    MB_ENTRIES=""
fi

# Compute new entries: HEAD entries minus MB entries (line-diff: present in HEAD, not in MB)
NEW_ENTRIES=""
while IFS= read -r entry; do
    [[ -z "$entry" ]] && continue
    # Check if this entry exists in MB_ENTRIES
    in_mb=0
    while IFS= read -r mb_entry; do
        [[ -z "$mb_entry" ]] && continue
        if [[ "$entry" == "$mb_entry" ]]; then
            in_mb=1
            break
        fi
    done <<< "$MB_ENTRIES"
    if [[ "$in_mb" -eq 0 ]]; then
        NEW_ENTRIES="${NEW_ENTRIES}${entry}"$'\n'
    fi
done <<< "$HEAD_ENTRIES"

# Append-only check: compute removed entries (MB entries absent from HEAD).
# A removed entry means an existing entry was modified or deleted, which
# violates the append-only audit trail requirement.
# Note: pr: is excluded from the canonical identity, so backfilling
# pr: null -> 439 on an existing entry does NOT count as a removal.
REMOVED_ENTRIES=""
while IFS= read -r mb_entry; do
    [[ -z "$mb_entry" ]] && continue
    in_head=0
    while IFS= read -r head_entry; do
        [[ -z "$head_entry" ]] && continue
        if [[ "$mb_entry" == "$head_entry" ]]; then
            in_head=1
            break
        fi
    done <<< "$HEAD_ENTRIES"
    if [[ "$in_head" -eq 0 ]]; then
        REMOVED_ENTRIES="${REMOVED_ENTRIES}${mb_entry}"$'\n'
    fi
done <<< "$MB_ENTRIES"

if [[ -n "${REMOVED_ENTRIES// /}" ]]; then
    REMOVED_LIST=$(printf '%s' "$REMOVED_ENTRIES" | grep -v '^$' | sed 's/^/  /' || true)
    fail_exception "Trajectory is append-only: existing entries were modified or removed. Removed canonical lines:
${REMOVED_LIST}
An exception must APPEND exactly one new entry; never edit existing ones."
fi

# Count new entries
NEW_ENTRY_COUNT=0
if [[ -n "${NEW_ENTRIES// /}" ]]; then
    NEW_ENTRY_COUNT=$(printf '%s' "$NEW_ENTRIES" | grep -c "^date=" || true)
fi

# ── Decision table ────────────────────────────────────────────────────────────

if [[ "$BODY_FACTOR" -eq 0 ]] && [[ "$NEW_ENTRY_COUNT" -eq 0 ]]; then
    # Both factors missing
    fail_exception "No exception declared: PR body has no 'loc-exception:' line and no new trajectory entry. Both are required."
fi

if [[ "$BODY_FACTOR" -eq 0 ]]; then
    # Body factor missing (ledger present but no body line)
    fail_exception "PR body factor missing: PR body contains no line matching 'loc-exception: <non-empty rationale>'. Unset/empty PR_BODY = 'no exception declared'."
fi

if [[ "$NEW_ENTRY_COUNT" -eq 0 ]]; then
    # Ledger factor missing (body present but no new entry)
    fail_exception "Trajectory factor missing: PR body has 'loc-exception:' but no new entry was found in the trajectory file at $(git rev-parse --abbrev-ref HEAD):${TRAJ_REL} vs merge-base."
fi

if [[ "$NEW_ENTRY_COUNT" -gt 1 ]]; then
    fail_exception "Trajectory factor invalid: found ${NEW_ENTRY_COUNT} new entries (exactly one new entry per PR is required; ledger must stay attributable). Remove the extra entries."
fi

# Exactly one new entry: extract its fields using tab delimiter (F3).
# Canonical format: date=<D><TAB>branch=<B><TAB>delta=<N><TAB>reason=<text>
NEW_ENTRY_LINE=$(printf '%s\n' "$NEW_ENTRIES" | grep "^date=" | head -1)

# Extract each field via parameter expansion (no subprocesses; handles tabs in reason).
# Strip the leading field-name prefix from each tab-delimited segment.
_remaining="$NEW_ENTRY_LINE"
DECLARED_DATE="${_remaining%%$'\t'*}"
DECLARED_DATE="${DECLARED_DATE#date=}"
_remaining="${_remaining#*$'\t'}"

DECLARED_BRANCH="${_remaining%%$'\t'*}"
DECLARED_BRANCH="${DECLARED_BRANCH#branch=}"
_remaining="${_remaining#*$'\t'}"

DECLARED_DELTA="${_remaining%%$'\t'*}"
DECLARED_DELTA="${DECLARED_DELTA#delta=}"

# reason= is everything after the last tab-delimited prefix
DECLARED_REASON=""
if [[ "$NEW_ENTRY_LINE" == *$'\t'reason=* ]]; then
    DECLARED_REASON="${NEW_ENTRY_LINE#*$'\t'reason=}"
fi

# FIX 2: Require non-empty date and branch identity fields on the new entry.
# Missing or empty date/branch makes the entry unattributable in the audit trail.
# Template for error messages: date/pr/branch/delta/reason (pr may be null).
_ENTRY_TEMPLATE="Entry template:
  - date: YYYY-MM-DD
    pr: <PR-number or null>
    branch: <branch-name>
    delta: <N>
    reason: \"<rationale>\""

if [[ -z "${DECLARED_DATE// /}" ]]; then
    fail_exception "Trajectory factor invalid: new entry is missing a non-empty 'date:' field. Each entry must be fully identified.
${_ENTRY_TEMPLATE}"
fi

if [[ -z "${DECLARED_BRANCH// /}" ]]; then
    fail_exception "Trajectory factor invalid: new entry is missing a non-empty 'branch:' field. Each entry must be fully identified.
${_ENTRY_TEMPLATE}"
fi

# F3: assert DECLARED_DELTA is numeric before comparing (non-numeric = likely an
# inline comment or malformed entry; fail with a clear message).
if ! [[ "$DECLARED_DELTA" =~ ^-?[0-9]+$ ]]; then
    fail_exception "Trajectory factor invalid: declared delta is not numeric ('${DECLARED_DELTA}'). Check for inline comments on the delta: line (e.g. 'delta: 42 # note' is not valid here)."
fi

# Validate reason is non-empty
if [[ -z "${DECLARED_REASON// /}" ]]; then
    fail_exception "Trajectory factor invalid: new entry has an empty 'reason:' field. Borrows must carry a rationale. Add a non-empty reason to the entry."
fi

# Validate declared delta matches computed delta (exact match required).
# F11: use printf '%s' to avoid backslash mangling in the reason text.
if [[ "$DECLARED_DELTA" != "$DELTA" ]]; then
    fail_exception "Trajectory delta mismatch: entry declares delta=${DECLARED_DELTA} but computed delta is ${DELTA}. Update the entry's delta: field to ${DELTA}."
fi

# ── Both factors satisfied: PASS with warning ─────────────────────────────────
# FIX 3: CUMULATIVE is computed live at HEAD, which already includes this PR's
# added lines. Adding DELTA again would double-count. The post-merge cumulative
# IS the current CUMULATIVE (no separate projection variable needed).
# F11: use printf '%s' for reason text (avoids backslash mangling in reason strings)
printf '%s\n' "PASS (exception declared): delta=+${DELTA}, reason: ${DECLARED_REASON}"
echo ""
echo "WARNING: LOC exception recorded. This is a borrow against the baseline."
printf '  declared reason : %s\n' "${DECLARED_REASON}"
echo "  delta           : +${DELTA}"
echo "  cumulative after this PR: ${CUMULATIVE} (this PR borrows +${DELTA} of it)"
echo ""

if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
    printf '::warning title=LOC exception::delta=+%s; reason=%s; cumulative after this PR=%s\n' \
        "${DELTA}" "${DECLARED_REASON}" "${CUMULATIVE}"
fi

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    {
        echo ""
        echo "### LOC Exception Declared"
        printf '%s\n' "- **Reason**: ${DECLARED_REASON}"
        echo "- **Delta**: +${DELTA}"
        echo "- **Cumulative after this PR**: ${CUMULATIVE} (this PR borrows +${DELTA} of it)"
    } >> "$GITHUB_STEP_SUMMARY"
fi

exit 0
