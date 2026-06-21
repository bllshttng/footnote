#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# control-plane-doc-colocation.sh
#
# Advisory CI nudge: warn (never block) when a PR changes control-plane code
# WITHOUT also touching docs/architecture/. Staleness defense — the docs that
# describe the control plane should travel in the same diff as the change.
#
# Same shape as loc-ratchet.sh (merge-base diff against BASE_REF), but:
#   - It is ADVISORY: it always exits 0. The signal is a ::warning annotation
#     plus a GitHub step-summary line, surfaced by a continue-on-error job.
#   - The control-plane path set is read from scripts/ci/loc-ratchet-manifest.yaml
#     (the SAME include: list the LOC ratchet uses), so the two checks can never
#     disagree about what counts as control plane.
#
# Base ref resolution (mirrors loc-ratchet.sh):
#   --base <ref>  overrides; otherwise BASE_REF env -> "origin/$BASE_REF".
#
# Exit code is always 0. Read-only. No state writes.
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="${LOC_RATCHET_MANIFEST:-${SCRIPT_DIR}/loc-ratchet-manifest.yaml}"
DOC_PREFIX="docs/architecture/"

# ── Args ────────────────────────────────────────────────────────────────────
BASE_OVERRIDE=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --base) BASE_OVERRIDE="${2:-}"; shift 2 ;;
        --base=*) BASE_OVERRIDE="${1#*=}"; shift ;;
        *) echo "advisory: unknown argument: $1" >&2; shift ;;
    esac
done

# ── Advisory degradation helper ──────────────────────────────────────────────
# Anything that would normally be a hard error is a soft no-op here: this check
# must never fail a PR. Print a notice and exit clean.
notice_and_exit() {
    echo "control-plane-doc-colocation: $1 (advisory; skipping)" >&2
    exit 0
}

[[ -f "$MANIFEST" ]] || notice_and_exit "manifest not found at $MANIFEST"

# ── Base ref ─────────────────────────────────────────────────────────────────
if [[ -n "$BASE_OVERRIDE" ]]; then
    BASE="$BASE_OVERRIDE"
elif [[ -n "${BASE_REF:-}" ]]; then
    BASE="origin/${BASE_REF}"
else
    notice_and_exit "no base ref (set BASE_REF or pass --base <ref>)"
fi

MB=$(git merge-base "$BASE" HEAD 2>/dev/null) \
    || notice_and_exit "cannot compute merge-base between '$BASE' and HEAD"

CHANGED=$(git diff --name-only "$MB" HEAD 2>/dev/null) \
    || notice_and_exit "git diff failed"

[[ -n "$CHANGED" ]] || { echo "PASS: no changed files."; exit 0; }

# ── Control-plane path set: include: block from the loc-ratchet manifest ──────
# Mirrors the manifest's match semantics (loc-ratchet-manifest.yaml header):
#   trailing "/" -> directory prefix; trailing "*" -> path-prefix glob;
#   otherwise -> exact file match.
INCLUDES=$(awk '
    /^include:/ {inblock=1; next}
    inblock && /^[A-Za-z]/ {inblock=0}
    inblock && /^[[:space:]]*-[[:space:]]/ {
        line=$0
        sub(/^[[:space:]]*-[[:space:]]*/, "", line)
        gsub(/"/, "", line)
        sub(/[[:space:]]+$/, "", line)
        if (line != "") print line
    }
' "$MANIFEST")

[[ -n "$INCLUDES" ]] || notice_and_exit "manifest has no include: entries"

# A file is "control plane" if it matches an include entry and is not a test file.
is_test_file() {
    local f="$1"
    case "$f" in
        */tests/*|tests/*) return 0 ;;
        */test_*|test_*) return 0 ;;
        *_test.*) return 0 ;;
    esac
    return 1
}

is_control_plane() {
    local f="$1" entry
    is_test_file "$f" && return 1
    while IFS= read -r entry; do
        [[ -n "$entry" ]] || continue
        case "$entry" in
            */)  [[ "$f" == "$entry"* ]] && return 0 ;;          # directory prefix
            *\*) [[ "$f" == "${entry%\*}"* ]] && return 0 ;;      # path-prefix glob
            *)   [[ "$f" == "$entry" ]] && return 0 ;;            # exact file
        esac
    done <<< "$INCLUDES"
    return 1
}

# ── Scan the diff ─────────────────────────────────────────────────────────────
CP_HITS=""
DOC_TOUCHED=0
while IFS= read -r f; do
    [[ -n "$f" ]] || continue
    [[ "$f" == "$DOC_PREFIX"* ]] && DOC_TOUCHED=1
    if is_control_plane "$f"; then
        CP_HITS="${CP_HITS}${f}"$'\n'
    fi
done <<< "$CHANGED"

# ── Verdict (advisory) ────────────────────────────────────────────────────────
if [[ -z "$CP_HITS" ]]; then
    echo "PASS: no control-plane paths changed."
    exit 0
fi

if [[ "$DOC_TOUCHED" -eq 1 ]]; then
    echo "PASS: control-plane changed and docs/architecture/ also touched."
    exit 0
fi

# Control plane touched, no architecture doc in the same diff -> advise.
CP_LIST=$(printf '%s' "$CP_HITS" | sed '/^$/d' | sed 's/^/  - /')
MSG="control-plane code changed without touching ${DOC_PREFIX}. Consider colocating the doc update in this PR."

printf '::warning title=Control-plane doc colocation::%s\n' "$MSG"
echo "ADVISORY: $MSG"
echo "Control-plane files changed in this PR:"
printf '%s\n' "$CP_LIST"

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    {
        echo "### ⚠️ Control-plane doc colocation (advisory)"
        echo ""
        echo "$MSG"
        echo ""
        echo "Control-plane files changed without a \`${DOC_PREFIX}\` update:"
        echo ""
        printf '%s\n' "$CP_LIST"
        echo ""
        echo "_This is advisory and never blocks the PR._"
    } >> "$GITHUB_STEP_SUMMARY"
fi

# Advisory: always succeed.
exit 0
