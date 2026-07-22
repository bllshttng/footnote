#!/usr/bin/env bash
# Write or update a memory entry with audit trail.
#
# Args:
#   --memory-dir DIR    Memory dir to write into
#   --session-id SID    Source session
#   --candidate JSON    Candidate object {type, name, description, body}
#   --empty-pass        Declare an explicit empty pass (no candidate). Writes
#                       only the gate artifact + event. The LLM ran the
#                       pre-promise pass and concluded nothing was memory-
#                       worthy this session. Phase 3 of loop-correctness-
#                       sweep (ab-83be25ea).
#
# Behavior:
#   - Compute target path: {memory-dir}/{type}_{slug(name)}.md
#   - If file exists: read existing frontmatter, compare description+body,
#       same -> skip (logs "deduped")
#       different -> append "Session {sid} update:" stanza to body
#   - If file new: write frontmatter + body with auto_generated: true and
#       source_session: {sid}
#   - Atomically update MEMORY.md index (tmp+rename)
#   - On any successful write OR --empty-pass, also writes a gate artifact at
#       ${ARTIFACTS_DIR:-${REPO_ROOT}/.fno/artifacts}/memory-{sid}.md
#       with frontmatter (phase, session_id, entries_written, approved) and
#       writes the memory artifact (gate flip removed in ab-d0337fbc).
#
# Exit codes:
#   0  wrote a new file or updated an existing entry (success), OR --empty-pass success
#   1  invalid candidate, missing args, or write failure (real error)
#   2  dedup hit - intentional no-op; the gate artifact mtime is NOT touched
#      and entries_written is NOT bumped (provenance must not record dedup as work)
#
# Distinct codes for dedup vs error so callers can tell intentional skips
# from real failures without grepping log lines.

set -euo pipefail

MEMORY_DIR=""
SESSION_ID=""
CANDIDATE_JSON=""
EMPTY_PASS=0
REPO_ROOT_ARG=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --memory-dir)  MEMORY_DIR="$2"; shift 2 ;;
        --session-id)  SESSION_ID="$2"; shift 2 ;;
        --candidate)   CANDIDATE_JSON="$2"; shift 2 ;;
        --empty-pass)  EMPTY_PASS=1; shift 1 ;;
        --repo-root)   REPO_ROOT_ARG="$2"; shift 2 ;;
        *) echo "write-memory-entry: unknown arg: $1" >&2; exit 1 ;;
    esac
done

log() { echo "write-memory-entry: $*" >&2; }

if [[ "$EMPTY_PASS" == "1" ]]; then
    [[ -z "$SESSION_ID" ]] && { log "--empty-pass requires --session-id"; exit 1; }
else
    [[ -z "$MEMORY_DIR" || -z "$SESSION_ID" || -z "$CANDIDATE_JSON" ]] && {
        log "missing required args"; exit 1; }
fi

# Resolve TWO independent roots. The old code resolved a single
# REPO_ROOT_RESOLVED from the SCRIPT's own dir (git -C SCRIPT_DIR) and used it
# for BOTH set-gate.sh AND the gate artifact + target-state.md. From a worktree
# those are different trees: the script lives in the fno plugin, the state
# lives in the target project. The conflation wrote the memory gate artifact
# into the fno repo and flipped (or failed to flip) the wrong/absent
# state file, so memory_pass_passed silently never landed for the running
# session. Split them:
#   - PLUGIN_ROOT  -> where set-gate.sh lives (the script's own tree).
#   - PROJECT_ROOT -> the target project whose .fno/ holds the artifact
#     and target-state.md. Precedence: --repo-root > cwd git toplevel > PWD.
SCRIPT_DIR_REAL="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ROOT="$(cd "$SCRIPT_DIR_REAL/../.." && pwd)"
# The grouping `! A || B` works under errexit because both arms are tests, not
# commands whose output gets concatenated.
if [[ -n "$REPO_ROOT_ARG" ]]; then
    PROJECT_ROOT="$REPO_ROOT_ARG"
elif ! PROJECT_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || [[ -z "$PROJECT_ROOT" ]]; then
    PROJECT_ROOT="$PWD"
fi
ARTIFACTS_DIR="${ARTIFACTS_DIR:-${PROJECT_ROOT}/.fno/artifacts}"

# emit_memory_gate ENTRIES_WRITTEN APPROVED
#
# Writes the memory gate artifact at ARTIFACTS_DIR/memory-${SESSION_ID}.md.
# set-gate.sh removed in Task 3.2 (control-plane collapse, ab-d0337fbc);
# the gate-flip leg is gone - the artifact alone serves the notification path.
emit_memory_gate() {
    local entries_written="$1" approved="$2"
    local artifact="${ARTIFACTS_DIR}/memory-${SESSION_ID}.md"
    mkdir -p "$ARTIFACTS_DIR" 2>/dev/null || {
        log "WARNING: could not create $ARTIFACTS_DIR; skipping gate artifact"
        return 0
    }
    local tmp
    tmp=$(mktemp "${artifact}.tmp.XXXXXX") || {
        log "WARNING: could not allocate tmp file for memory artifact"
        return 0
    }
    cat > "$tmp" <<ART_EOF
---
phase: memory
session_id: ${SESSION_ID}
entries_written: ${entries_written}
approved: ${approved}
completed_at: $(date -u +%Y-%m-%dT%H:%M:%SZ)
---
ART_EOF
    if ! mv "$tmp" "$artifact"; then
        rm -f "$tmp"
        log "WARNING: could not finalize $artifact"
        return 0
    fi
    return 0
}

# Empty-pass branch: no memory file write; just the gate artifact + event.
# entries_written: 0, approved: true (the LLM ran the pass; result was nothing).
if [[ "$EMPTY_PASS" == "1" ]]; then
    emit_memory_gate 0 true
    log "empty pass: gate artifact written for session $SESSION_ID"
    exit 0
fi

mkdir -p "$MEMORY_DIR"

# Parse candidate JSON, slugify name, compute target path.
PARSED_OUTPUT=$(MEM_DIR="$MEMORY_DIR" CANDIDATE="$CANDIDATE_JSON" python3 - <<'PYEOF'
import os, json, re, sys
try:
    c = json.loads(os.environ["CANDIDATE"])
except Exception as e:
    print(f"ERR: invalid JSON: {e}")
    sys.exit(1)
t = c.get("type", "")
n = c.get("name", "")
d = c.get("description", "")
b = c.get("body", "")
# All four fields must be STRINGS, not merely truthy. A non-string body (e.g.
# {"body": {"oops": 1}}) passes a bare truthiness check but raises TypeError at
# sys.stdout.write(b) AFTER the header lines are out - partial output the shell
# must never mistake for a successful parse (codex P2 on PR #435). Reject it
# here with a structured ERR instead.
bad = [k for k, v in (("type", t), ("name", n), ("description", d), ("body", b))
       if not isinstance(v, str)]
if bad:
    print(f"ERR: non-string field(s): {', '.join(bad)} (type/name/description/body must all be strings)")
    sys.exit(1)
if t not in ("feedback", "project", "reference", "user") or not n or not d or not b:
    missing = [k for k, v in (("name", n), ("description", d), ("body", b)) if not v]
    if t not in ("feedback", "project", "reference", "user"):
        print(f"ERR: invalid type {t!r} (must be one of feedback|project|reference|user)")
    else:
        print(f"ERR: missing required field(s): {', '.join(missing)}")
    sys.exit(1)
slug = re.sub(r'[^a-z0-9_]+', '_', n.lower()).strip('_')
target = os.path.join(os.environ["MEM_DIR"], f"{t}_{slug}.md")
print(f"TYPE:{t}")
print(f"NAME:{n}")
print(f"DESC:{d}")
print(f"SLUG:{slug}")
print(f"TARGET:{target}")
sys.stdout.write("BODY:\n")
sys.stdout.write(b)
PYEOF
) || PARSE_RC=$?
# The python validator exits 1 on a bad candidate AND prints a structured
# `ERR:` line to stdout (captured above). Without a guard, `set -e` aborts the
# assignment here BEFORE the loop below surfaces that ERR - the caller then
# sees a SILENT exit 1 (cv-c97d73e3). But a blanket `|| true` over-corrects:
# it also masks system failures (python3 missing = 127, interpreter crash) and
# would let a parser that died AFTER partial output read as a successful parse
# (gemini medium + codex P2 on PR #435). So: capture the rc, let ONLY the
# structured-rejection path (rc=1, ERR line present) flow into the loop, and
# abort on anything else with the real exit code.
PARSE_RC="${PARSE_RC:-0}"
if [[ "$PARSE_RC" -ne 0 && "$PARSE_RC" -ne 1 ]]; then
    log "candidate parser failed unexpectedly (rc=$PARSE_RC; python3 missing or interpreter failure?)"
    exit "$PARSE_RC"
fi

# Bash 3.2 portable: read line-by-line via while loop, no mapfile.
ENTRY_TYPE=""; ENTRY_NAME=""; ENTRY_DESC=""; ENTRY_SLUG=""; TARGET=""
ENTRY_BODY=""
in_body=0
while IFS= read -r line; do
    if [[ "$in_body" == "1" ]]; then
        ENTRY_BODY+="$line"$'\n'
        continue
    fi
    case "$line" in
        ERR:*)         log "${line#ERR: }"; exit 1 ;;
        TYPE:*)        ENTRY_TYPE="${line#TYPE:}" ;;
        NAME:*)        ENTRY_NAME="${line#NAME:}" ;;
        DESC:*)        ENTRY_DESC="${line#DESC:}" ;;
        SLUG:*)        ENTRY_SLUG="${line#SLUG:}" ;;
        TARGET:*)      TARGET="${line#TARGET:}" ;;
        BODY:)         in_body=1 ;;
    esac
done <<< "$PARSED_OUTPUT"

# The loop exits 1 the moment it sees a structured `ERR:` line, so reaching
# here with a nonzero PARSE_RC means python died with rc=1 (an uncaught
# exception also exits 1) AFTER emitting partial output but WITHOUT a
# structured ERR. Partial headers + a truncated body must never be treated as
# a successful parse - that would write a broken memory file and flip the
# memory gate on a crash (codex P2 on PR #435).
if [[ "$PARSE_RC" -ne 0 ]]; then
    log "candidate parser exited rc=$PARSE_RC without a structured ERR (crashed mid-output); aborting"
    exit 1
fi

[[ -z "$TARGET" ]] && { log "target path empty"; exit 1; }

INDEX_FILE="$MEMORY_DIR/MEMORY.md"
FILENAME=$(basename "$TARGET")

now_iso() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }

write_atomic() {
    local target="$1" content="$2"
    local tmp
    tmp=$(mktemp "$(dirname "$target")/.tmp.write.XXXXXX")
    printf '%s' "$content" > "$tmp"
    mv "$tmp" "$target"
}

frontmatter_for_new() {
    cat <<EOF
---
name: $ENTRY_NAME
description: $ENTRY_DESC
type: $ENTRY_TYPE
auto_generated: true
source_session: $SESSION_ID
created_at: $(now_iso)
---
EOF
}

# Read existing entry (if any) to decide dedup vs update.
if [[ -f "$TARGET" ]]; then
    existing_desc=""
    existing_body=""
    in_fm=0; fm_seen=0
    while IFS= read -r line || [[ -n "$line" ]]; do
        if [[ "$line" == "---" ]]; then
            if [[ "$fm_seen" == "0" ]]; then
                in_fm=1; fm_seen=1; continue
            elif [[ "$in_fm" == "1" ]]; then
                in_fm=0; continue
            fi
        fi
        if [[ "$in_fm" == "1" ]]; then
            case "$line" in
                description:*) existing_desc="${line#description:}"; existing_desc="${existing_desc# }" ;;
            esac
        else
            existing_body+="$line"$'\n'
        fi
    done < "$TARGET"

    if [[ "$(printf '%s' "$existing_body")" == "$(printf '%s' "$ENTRY_BODY")" \
          && "$existing_desc" == "$ENTRY_DESC" ]]; then
        log "deduped: $FILENAME (identical)"
        # Dedup is an intentional no-op for the memory file and the
        # provenance event - we do NOT bump entries_written or emit a
        # second phase_transition (the plan's AC2-ERR pins this). But
        # under strict mode the LLM still needs the gate artifact to
        # land so the pre-promise pass counts as completed. If no
        # artifact exists yet (e.g. dedup is the LLM's first writer
        # call this session and only candidate), drop a zero-entry
        # passing artifact so strict mode does not block on a no-op.
        # This preserves the spirit of the dedup rule (no provenance
        # pollution: events.jsonl gets one event per gate flip per
        # session, not per dedup) while letting honest sessions reach
        # COMPLETE. When an artifact already exists for this session,
        # leave its mtime + entries_written untouched (the early gate
        # is already satisfied; nothing more to do).
        EXISTING_ARTIFACT="${ARTIFACTS_DIR}/memory-${SESSION_ID}.md"
        if [[ ! -f "$EXISTING_ARTIFACT" ]]; then
            emit_memory_gate 0 true
            log "dedup-on-first-call: emitted zero-entry gate artifact for session $SESSION_ID"
        fi
        exit 2
    fi

    # Update path: append "Session {sid} update" stanza, preserve original.
    appended_body=$(printf '%s\n\n## Session %s update\n\n%s' \
        "$(printf '%s' "$existing_body")" "$SESSION_ID" "$ENTRY_BODY")
    # Preserve existing frontmatter; only update description if new is more specific.
    # Use ENVIRON instead of `awk -v` because `-v var="$value"` interprets
    # backslash escapes (a `\n` literal in the description would become an
    # actual newline). ENVIRON passes the value through verbatim.
    new_fm=$(DESC="$ENTRY_DESC" awk '
        BEGIN { in_fm=0; seen=0; desc=ENVIRON["DESC"] }
        $0 == "---" {
            if (seen == 0) { in_fm=1; seen=1; print; next }
            else if (in_fm == 1) { in_fm=0; print; exit }
        }
        in_fm == 1 && $0 ~ /^description:/ {
            if (length(desc) > length($0) - length("description: ")) {
                print "description: " desc
            } else {
                print
            }
            next
        }
        in_fm == 1 { print }
    ' "$TARGET")
    write_atomic "$TARGET" "${new_fm}
${appended_body}"
    log "updated: $FILENAME (appended session $SESSION_ID stanza)"
    # MEMORY.md already indexes this file - no append needed.
    # Bump entries_written on the gate artifact (Phase 3 task 3.1).
    PRIOR_ENTRIES=0
    EXISTING_ARTIFACT="${ARTIFACTS_DIR}/memory-${SESSION_ID}.md"
    if [[ -f "$EXISTING_ARTIFACT" ]]; then
        PRIOR_ENTRIES=$(grep -E '^entries_written:[[:space:]]*' "$EXISTING_ARTIFACT" 2>/dev/null \
            | head -1 | sed -E 's/^entries_written:[[:space:]]*//' | tr -d ' ')
        [[ "$PRIOR_ENTRIES" =~ ^[0-9]+$ ]] || PRIOR_ENTRIES=0
    fi
    emit_memory_gate $((PRIOR_ENTRIES + 1)) true
    exit 0
fi

# New file path.
fm=$(frontmatter_for_new)
write_atomic "$TARGET" "${fm}
${ENTRY_BODY}"
log "wrote: $FILENAME"
# Gate artifact + event for the new-file path.
PRIOR_ENTRIES=0
EXISTING_ARTIFACT="${ARTIFACTS_DIR}/memory-${SESSION_ID}.md"
if [[ -f "$EXISTING_ARTIFACT" ]]; then
    PRIOR_ENTRIES=$(grep -E '^entries_written:[[:space:]]*' "$EXISTING_ARTIFACT" 2>/dev/null \
        | head -1 | sed -E 's/^entries_written:[[:space:]]*//' | tr -d ' ')
    [[ "$PRIOR_ENTRIES" =~ ^[0-9]+$ ]] || PRIOR_ENTRIES=0
fi
emit_memory_gate $((PRIOR_ENTRIES + 1)) true

# MEMORY.md atomic update: append "- [Title](file.md) - description" if not present.
INDEX_LINE="- [$ENTRY_NAME]($FILENAME) - $ENTRY_DESC"
if [[ ! -f "$INDEX_FILE" ]]; then
    write_atomic "$INDEX_FILE" "# Memory Index

$INDEX_LINE
"
else
    # Idempotent: skip if filename already indexed.
    if ! grep -qE "\\($(printf '%s' "$FILENAME" | sed 's/[][\\.*^$/]/\\&/g')\\)" "$INDEX_FILE" 2>/dev/null; then
        tmp=$(mktemp "$(dirname "$INDEX_FILE")/.tmp.idx.XXXXXX")
        cat "$INDEX_FILE" > "$tmp"
        # Ensure trailing newline before append.
        [[ -n "$(tail -c 1 "$tmp" 2>/dev/null)" ]] && printf '\n' >> "$tmp"
        printf '%s\n' "$INDEX_LINE" >> "$tmp"
        mv "$tmp" "$INDEX_FILE"
    fi
fi

exit 0
