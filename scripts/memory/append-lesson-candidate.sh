#!/usr/bin/env bash
# append-lesson-candidate.sh - dual-emit a load-bearing project lesson to the
# home-keyed lesson-candidates.jsonl, the staging file for the AGENTS.md
# pitfalls corpus.
#
# The memory pass writes session-continuity entries to private agent memory
# (write-memory-entry.sh). A candidate that clears the oss-fix-not-memory
# discriminator ("would a stranger cloning the repo need this?") is ALSO appended
# here, so it reaches the corpus on the next promotion PR instead of draining to
# a fix mailed to yourself. Home-keyed ($HOME/.fno/...): worktree-independent,
# harness-readable, no post-merge git-commit problem. Promotion into AGENTS.md is
# always a reviewed PR, never automatic.
#
# Mirrors the events.jsonl append convention: mkdir the dir, jq-build one line,
# append under O_APPEND (atomic for small line writes). A failed append warns to
# stderr and exits 0 - it NEVER blocks the memory write or the merge.
#
# Args:
#   --candidate JSON   Candidate object {type, name, description, body}
#   --session-id SID   Source session id
#   --file PATH        Override target (default $HOME/.fno/lesson-candidates.jsonl)

# No `set -e`: a failed append must not abort the caller's memory write.
set -uo pipefail

CANDIDATE_JSON=""
SESSION_ID=""
FILE="${LESSON_CANDIDATES_FILE:-$HOME/.fno/lesson-candidates.jsonl}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --candidate)  CANDIDATE_JSON="$2"; shift 2 ;;
        --session-id) SESSION_ID="$2"; shift 2 ;;
        --file)       FILE="$2"; shift 2 ;;
        *) echo "append-lesson-candidate: unknown arg: $1 (non-fatal)" >&2; exit 0 ;;
    esac
done

warn() { echo "append-lesson-candidate: $*" >&2; }

if [[ -z "$CANDIDATE_JSON" ]]; then
    warn "no --candidate given; not staged (non-fatal)"
    exit 0
fi

if ! command -v jq >/dev/null 2>&1; then
    warn "jq missing; lesson candidate not staged (non-fatal)"
    exit 0
fi

mkdir -p "$(dirname "$FILE")" 2>/dev/null || true

line="$(jq -nc \
    --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    --arg sid "$SESSION_ID" \
    --argjson c "$CANDIDATE_JSON" \
    '{ts: $ts, source_session: $sid,
      type: ($c.type // "unknown"), name: ($c.name // ""),
      description: ($c.description // ""), body: ($c.body // "")}' \
    2>/dev/null)" || {
    warn "jq rejected the candidate payload; not staged (non-fatal)"
    exit 0
}

if printf '%s\n' "$line" >> "$FILE" 2>/dev/null; then
    exit 0
fi

name="$(printf '%s' "$CANDIDATE_JSON" | jq -r '.name // "?"' 2>/dev/null)"
warn "append to $FILE failed; lesson candidate '${name}' not staged (non-fatal)"
exit 0
