#!/usr/bin/env bash
# check-product-md.sh - PRODUCT.md prereq check for /blueprint (Phase 02.1)
#
# Usage: check-product-md.sh <plan.md | plan-dir>
#
# Checks whether the plan (a single .md - the only authored shape - or a legacy
# folder) uses executor: impeccable (plan-level or per-task). If so, searches for
# a valid PRODUCT.md in the project root and fallback locations. If missing or
# stale, writes a prerequisites: block to the plan doc's frontmatter (the file
# itself, or 00-INDEX.md for a folder) and warns on stderr. Plan still ships.
#
# Exit: always 0 (this is heads-up only; the hard-block lives in operator at
# dispatch time per Phase 03).

set -euo pipefail
# Cleanup tempfiles and lock dir on any exit path (including set -e failures)
trap 'rm -f "${OUTFILE:-}" "${PREREQ_TMP:-}"; rm -rf "${LOCK_DIR:-}"' EXIT

PLAN_DIR="${1:?Usage: check-product-md.sh <plan.md | plan-dir>}"
# The plan is a single .md (the only authored shape) or a legacy folder. Resolve
# the doc to read/write and the on-filesystem dir for temp files + git root.
if [[ -f "$PLAN_DIR" ]]; then
    INDEX_FILE="$PLAN_DIR"
    PLAN_TMP_DIR="$(dirname "$PLAN_DIR")"
else
    INDEX_FILE="$PLAN_DIR/00-INDEX.md"
    PLAN_TMP_DIR="$PLAN_DIR"
fi
REPO_ROOT="${REPO_ROOT:-$(git -C "$PLAN_TMP_DIR" rev-parse --show-toplevel 2>/dev/null || pwd)}"
# Lock dir variable - initialized empty; set only if the lock is acquired below.
LOCK_DIR=""

# -------------------------------------------------------------------
# 1. Detect whether this plan uses executor: impeccable
# -------------------------------------------------------------------

_uses_impeccable() {
    # Plan-level executor: impeccable in the doc/index frontmatter.
    if [[ -f "$INDEX_FILE" ]]; then
        if awk '/^---/{c++; if(c==2) exit; next} c==1{print}' "$INDEX_FILE" \
                | grep -qE '^executor:[[:space:]]*impeccable[[:space:]]*$'; then
            return 0
        fi
    fi
    if [[ -d "$PLAN_DIR" ]]; then
        # Legacy folder plan: per-task overrides live in numbered phase files.
        for phase_file in "$PLAN_DIR"/[0-9][0-9]*.md; do
            [[ -f "$phase_file" ]] || continue
            [[ "$(basename "$phase_file")" == "00-INDEX.md" ]] && continue
            if grep -qE '^[[:space:]]*executor:[[:space:]]*impeccable[[:space:]]*$' "$phase_file" 2>/dev/null; then
                return 0
            fi
        done
    elif [[ -f "$INDEX_FILE" ]]; then
        # Single-doc plan: per-task overrides live in task blocks in the doc body.
        if grep -qE '^[[:space:]]*executor:[[:space:]]*impeccable[[:space:]]*$' "$INDEX_FILE" 2>/dev/null; then
            return 0
        fi
    fi
    return 1
}

if ! _uses_impeccable; then
    exit 0
fi

# -------------------------------------------------------------------
# 2. Search for a valid PRODUCT.md (three fallback locations)
# -------------------------------------------------------------------

PRODUCT_MD_FOUND=""
PRODUCT_MD_PATH=""

_is_valid_product_md() {
    local path="$1"
    [[ -f "$path" ]] || return 1
    local size
    size=$(wc -c < "$path" | tr -d ' ')
    [[ "$size" -ge 200 ]] || return 1
    # [TODO] dominance check (matches orchestrator.py is_product_md_stale).
    # Each "[TODO]" token is 6 bytes; if tokens occupy more than 25% of the
    # file content (todo_count * 6 > size / 4), treat as a placeholder stub.
    # Without this check the /blueprint gate would silently pass a stub that
    # /do waves's dispatch gate then hard-blocks on - the two layers must
    # agree on staleness or defense-in-depth degenerates into surprise.
    # Use grep -o ... | wc -l (not grep -c) so multiple [TODO] on one line
    # are counted individually, matching Python's content.count("[TODO]").
    local todo_count
    todo_count=$(grep -o '\[TODO\]' "$path" 2>/dev/null | wc -l | tr -d ' ')
    : "${todo_count:=0}"
    if (( todo_count * 6 * 4 > size )); then
        return 1
    fi
    return 0
}

for candidate in \
    "$REPO_ROOT/PRODUCT.md" \
    "$REPO_ROOT/.agents/context/PRODUCT.md" \
    "$REPO_ROOT/docs/PRODUCT.md"
do
    if _is_valid_product_md "$candidate"; then
        PRODUCT_MD_FOUND="yes"
        PRODUCT_MD_PATH="$candidate"
        break
    fi
done

if [[ -n "$PRODUCT_MD_FOUND" ]]; then
    # Valid PRODUCT.md found - nothing to do
    exit 0
fi

# -------------------------------------------------------------------
# 3. PRODUCT.md missing or stale: write prerequisites block + warn
# -------------------------------------------------------------------

# Print warning to stderr
echo "warning: this plan locks executor: impeccable but no PRODUCT.md was found." >&2
echo "Run /impeccable teach before /target dispatch, or /do waves will hard-block." >&2

# Write prerequisites: block to 00-INDEX.md frontmatter
# We inject it after the first --- line (opening fence), before the closing ---.
# If prerequisites: already present, skip to avoid duplication.
if [[ ! -f "$INDEX_FILE" ]]; then
    exit 0
fi

# Inject prerequisites block into frontmatter, protected by a per-plan lock so
# concurrent /blueprint invocations against the same plan dir don't race and corrupt
# the frontmatter. Locking strategy:
#   - Use mkdir as an atomic lock primitive (POSIX-guaranteed, portable to macOS).
#     mkdir succeeds only in one process; others spin-wait with a short sleep.
#   - Re-check the dedup guard AFTER acquiring the lock (TOCTOU prevention).
#   - Use mktemp -p PLAN_DIR so the temp file is on the same filesystem as
#     INDEX_FILE, guaranteeing mv is a rename(2)-atomic operation.
#   - Lock dir is removed in the EXIT trap below to handle SIGTERM/SIGKILL cleanup.
LOCK_DIR="${INDEX_FILE}.lock.d"
_acquired_lock=0
# Spin-wait: up to 10 attempts x 0.1s = 1s max wait
for _i in 1 2 3 4 5 6 7 8 9 10; do
    if mkdir "$LOCK_DIR" 2>/dev/null; then
        _acquired_lock=1
        break
    fi
    sleep 0.1
done

if [[ $_acquired_lock -eq 0 ]]; then
    # Could not acquire lock; skip injection to avoid corruption.
    # The plan still ships (this is heads-up only).
    exit 0
fi

# TOCTOU re-check inside the lock
if grep -q '^prerequisites:' "$INDEX_FILE" 2>/dev/null; then
    exit 0  # Another concurrent process injected it first
fi

PREREQ_TMP=$(mktemp -p "$PLAN_TMP_DIR")
cat > "$PREREQ_TMP" << 'PREREQ_EOF'
prerequisites:
  - kind: file
    path: PRODUCT.md
    missing_reason: "required by /impeccable's setup gate; runtime will hard-block at dispatch"
PREREQ_EOF

OUTFILE=$(mktemp -p "$PLAN_TMP_DIR")
awk -v prereq_file="$PREREQ_TMP" '
    /^---/ { count++ }
    count == 2 && !inserted {
        while ((getline line < prereq_file) > 0) { print line }
        close(prereq_file)
        inserted = 1
    }
    { print }
' "$INDEX_FILE" > "$OUTFILE"
mv "$OUTFILE" "$INDEX_FILE"

exit 0
