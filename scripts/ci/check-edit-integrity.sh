#!/usr/bin/env bash
#
# The CI door for the edit-integrity checks: opencode and agy workers have
# no edit-time hook, so the same native entry runs over the branch diff
# and names what an edit broke. Advisory in preflight (the leg records a
# status, never flips the exit code); this script's exit is the entry's.

set -uo pipefail

PATH="${PATH:+$PATH:}/usr/bin:/bin"
export PATH

REPO="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo "check-edit-integrity: not inside a git repository" >&2
    exit 3
}

BASE="${EDIT_INTEGRITY_BASE:-}"
if [[ -z "$BASE" ]]; then
    BASE="$(git -C "$REPO" merge-base origin/main HEAD 2>/dev/null || true)"
fi
if [[ -z "$BASE" ]] || ! git -C "$REPO" rev-parse --verify --quiet "$BASE^{commit}" >/dev/null 2>&1; then
    echo "check-edit-integrity: no resolvable base (set EDIT_INTEGRITY_BASE)" >&2
    exit 2
fi

FILES=()
while IFS= read -r -d '' f; do
    FILES+=("$f")
done < <(git -C "$REPO" diff --name-only --diff-filter=AMR -z "$BASE" HEAD)
if [[ ${#FILES[@]} -eq 0 ]]; then
    echo "check-edit-integrity: no changed files"
    exit 0
fi

# Run-as-probe: exit 0 or 1 is an answer (1 carries blocking findings);
# 2 or higher means a build without the entry, so the next candidate is
# tried. debug before release: preflight reaches here after the cargo
# legs built the debug bin.
ANSWER_RC=9
for candidate in \
    "$REPO/crates/fno-agents/target/debug/fno-agents" \
    "$REPO/crates/fno-agents/target/release/fno-agents" \
    "${FNO_AGENTS_BIN:-}" \
    "$(command -v fno-agents 2>/dev/null || true)"; do
    [[ -n "$candidate" ]] || continue
    [[ -x "$candidate" ]] || continue
    (cd "$REPO" && "$candidate" hook edit-integrity --base "$BASE" -- "${FILES[@]}")
    ANSWER_RC=$?
    [[ "$ANSWER_RC" -le 1 ]] && break
    ANSWER_RC=9
done

if [[ "$ANSWER_RC" -le 1 ]]; then
    exit "$ANSWER_RC"
fi
echo "check-edit-integrity: no fno-agents answered hook edit-integrity; run cargo build --bin fno-agents (or fno doctor update)" >&2
exit 3
