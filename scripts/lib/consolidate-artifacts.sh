#!/usr/bin/env bash
# consolidate-artifacts.sh - per-PR artifact consolidator.
#
# Collects every phase artifact for the current target session into ONE
# per-PR file named {YYYYMMDD}{SS}-{pr#}-artifact.md under
# .fno/artifacts/ (which setup-worktree.sh symlinks to canonical
# so consolidated files persist across worktree archival).
#
# Master frontmatter aggregates the per-phase frontmatter; the body is
# the concatenated per-phase bodies (each prefixed with `## <phase>`).
# Per-phase artifact files are NOT deleted - they remain the stop hook's
# gate verification target; consolidation is purely additive retrospective
# data.
#
# Usage:
#   bash scripts/lib/consolidate-artifacts.sh
#   SESSION_ID=<id> PR_NUMBER=<num> bash scripts/lib/consolidate-artifacts.sh
#
# Args (both optional; default-read from .fno/target-state.md):
#   SESSION_ID  target session_id (matches the artifact file suffix)
#   PR_NUMBER   PR number (used in the consolidated filename)
#   STATE_DIR   override of <repo>/.fno (testing only)
#
# Exit codes:
#   0    consolidated (or skipped because inputs missing)
#   1    write failed OR >99 sessions per (pr, day) - refuses to clobber
#
# Stdout: a single "consolidated: <path>" line on success, nothing on skip.

set -uo pipefail

STATE_DIR="${STATE_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)/.fno}"

read_state_field() {
    local field="$1" state_file="$STATE_DIR/target-state.md"
    [[ -r "$state_file" ]] || return 0
    sed -n "s/^[[:space:]]*${field}:[[:space:]]*//p" "$state_file" \
        | head -1 | tr -d '"' | tr -d "'" | tr -d ' '
}

SESSION_ID="${SESSION_ID:-$(read_state_field session_id)}"
PR_NUMBER="${PR_NUMBER:-$(read_state_field pr_number)}"

if [[ -z "$SESSION_ID" || "$SESSION_ID" == "null" \
      || -z "$PR_NUMBER" || "$PR_NUMBER" == "null" ]]; then
    echo "consolidate-artifacts: missing session_id or pr_number; skipping" >&2
    exit 0
fi

# Injection guard: pr_number must be a positive integer; session_id must
# match the target session-id shape (timestamp-counter-hex). Refuse anything
# that would let a malicious state-file inject path components.
if ! [[ "$PR_NUMBER" =~ ^[1-9][0-9]*$ ]]; then
    echo "consolidate-artifacts: invalid pr_number '$PR_NUMBER'" >&2
    exit 1
fi
if ! [[ "$SESSION_ID" =~ ^[A-Za-z0-9._-]+$ ]]; then
    echo "consolidate-artifacts: invalid session_id '$SESSION_ID'" >&2
    exit 1
fi

# Per-phase artifacts live in $ARTIFACTS_DIR (worktree-local by design - the
# stop hook's archive sweep moves prior-session per-phase artifacts under
# the plan dir at session end). The CONSOLIDATED retrospective files live
# in a subdirectory that setup-worktree.sh symlinks to canonical so they
# persist across worktree archival. This split is load-bearing: linking
# the whole artifacts dir to canonical (the original Task 5 design) means
# worktree A's archive sweep would move worktree B's active per-phase
# artifacts AND every prior consolidated file out from under them, breaking
# B's gate verification and defeating the "artifacts by PR" persistence
# goal. Codex flagged this as P1 on PR #320 (round 2). See
# scripts/lib/archive-artifacts.sh stale-sweep loop for the foreign-sid
# filter that triggers the race.
ARTIFACTS_DIR="$STATE_DIR/artifacts"
CONSOLIDATED_DIR="$ARTIFACTS_DIR/consolidated"
mkdir -p "$ARTIFACTS_DIR" "$CONSOLIDATED_DIR"

# Collect per-phase artifact files for this session. Sort lexicographically
# so the consolidated body has a deterministic phase order.
shopt -s nullglob
PHASE_FILES=( "$ARTIFACTS_DIR"/*-"$SESSION_ID".md )
shopt -u nullglob

if [[ ${#PHASE_FILES[@]} -eq 0 ]]; then
    echo "consolidate-artifacts: no phase artifacts for session $SESSION_ID; skipping" >&2
    exit 0
fi

IFS=$'\n' PHASE_FILES=( $(printf '%s\n' "${PHASE_FILES[@]}" | LC_ALL=C sort) )
unset IFS

# Reserve next available SS for today + this PR via atomic noclobber create.
# A naive check-then-write (`-e` test then `mv`) is TOCTOU: two consolidators
# for the same (PR, UTC day) can both observe SS=N as free, then the second
# `mv` silently clobbers the first. `set -o noclobber` + redirect into the
# candidate path is an atomic create-or-fail: only one of the racing
# processes can successfully reserve a given slot, so each gets a unique
# `{YYYYMMDD}{SS}` filename. Reservation creates a zero-byte placeholder we
# overwrite via `mv` at the end - mv replaces, which is fine for a file we
# own. CONSOLIDATED_DIR is symlinked to canonical (per setup-worktree.sh)
# so the noclobber check sees all same-day reservations from any worktree.
DATE_PREFIX="$(date -u +%Y%m%d)"
SS=0
OUTPUT=""
while [[ "$SS" -le 99 ]]; do
    candidate="$CONSOLIDATED_DIR/${DATE_PREFIX}$(printf '%02d' "$SS")-${PR_NUMBER}-artifact.md"
    # Run in a subshell so `set -o noclobber` doesn't leak into the rest of
    # the script. `: > $candidate` creates an empty file atomically and
    # fails (rc!=0) if the path already exists.
    if (set -o noclobber; : > "$candidate") 2>/dev/null; then
        OUTPUT="$candidate"
        break
    fi
    SS=$((SS + 1))
done

if [[ -z "$OUTPUT" ]]; then
    echo "consolidate-artifacts: >99 sessions for PR $PR_NUMBER on $DATE_PREFIX; refusing to clobber" >&2
    exit 1
fi
SS_PADDED="$(printf '%02d' "$SS")"

# Strip the leading-and-trailing `---` frontmatter from a phase file and
# emit the body. Tolerates files that have no frontmatter (no `---` at
# all): in that case we print everything.
strip_frontmatter() {
    awk '
        BEGIN { in_fm = 0; seen_fm = 0; saw_open = 0 }
        NR == 1 && /^---$/ { in_fm = 1; seen_fm = 1; saw_open = 1; next }
        in_fm && /^---$/   { in_fm = 0; next }
        in_fm              { next }
        { print }
    ' "$1"
}

# Build into a tempfile within the destination dir for atomic rename.
# Fail loudly if mktemp can't create the tempfile (disk full, perms): an
# empty TMP would turn the later `> "$TMP"` redirect into `> ""` which is
# a runtime error. The injected `|| exit 1` short-circuits before the trap.
TMP="$(mktemp "${OUTPUT}.tmp.XXXXXX")" || {
    echo "consolidate-artifacts: mktemp failed in $CONSOLIDATED_DIR" >&2
    # The noclobber-reserved zero-byte OUTPUT placeholder is now orphaned;
    # remove it so the SS slot it reserved doesn't permanently bloat the
    # filename space.
    rm -f "$OUTPUT"
    exit 1
}
# Trap cleans up both TMP and the noclobber-reserved OUTPUT placeholder if
# the script dies between reservation and a successful mv. The untrap below
# (after mv) preserves the now-populated OUTPUT on the happy path.
trap 'rm -f "$TMP" "$OUTPUT"' EXIT

{
    echo "---"
    echo "pr_number: $PR_NUMBER"
    echo "session_id: $SESSION_ID"
    echo "consolidated_at: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "phases:"
    for phase_file in "${PHASE_FILES[@]}"; do
        # SESSION_ID is validated upstream to [A-Za-z0-9._-]+, so no path
        # separators or glob metacharacters - safe for bash parameter
        # expansion. Avoids per-iteration basename + sed forks.
        phase="${phase_file##*/}"
        phase="${phase%-"$SESSION_ID".md}"
        echo "  - $phase"
    done
    echo "---"
    echo ""
    for phase_file in "${PHASE_FILES[@]}"; do
        phase="${phase_file##*/}"
        phase="${phase%-"$SESSION_ID".md}"
        echo "## $phase"
        echo ""
        strip_frontmatter "$phase_file"
        echo ""
    done
} > "$TMP"

if ! mv "$TMP" "$OUTPUT"; then
    echo "consolidate-artifacts: write failed for $OUTPUT" >&2
    exit 1
fi
trap - EXIT

echo "consolidated: $OUTPUT"
