#!/usr/bin/env bash
#
# PostToolUse on Edit|Write: name what the edit just broke, as
# additionalContext the model reads in the same turn.
#
# A thin shim over the native entry `fno-agents hook edit-integrity` (the
# checks live in crates/fno-agents/src/hook/edit_integrity.rs). It never
# fails the edit: every path here exits 0, and a hook that has nothing to
# say prints nothing.
#
# The entry is probed per candidate binary, in the test-run-guard order:
# PATH first, then FNO_AGENTS_BIN, then the checkout's own target dirs. An
# exit of 0 or 1 is an answer; 2 or higher means a build without the
# entry, so the next candidate is tried and silence wins if none answers.

set -uo pipefail

PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

PAYLOAD="$(cat 2>/dev/null || true)"
[ -n "$PAYLOAD" ] || exit 0

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)" || exit 0
# shellcheck source=lib/write-targets.sh
source "$HOOK_DIR/lib/write-targets.sh" 2>/dev/null || exit 0

TARGETS=""
TARGETS="$(payload_write_targets "$PAYLOAD" 2>/dev/null || true)"
[ -n "$TARGETS" ] || exit 0

# Claude hands back the pre-edit text in tool_response.originalFile; with
# exactly one path it becomes the baseline, so a cut in an uncommitted
# file is caught and an intended drop is reported once. The text lands in
# the file byte-exactly - jq -j adds no newline and command substitution
# is never used, because $() strips the trailing newline and an
# unterminated-looking baseline would silence the cut-short finding. A
# codex apply_patch payload, a new file, or a payload without the field
# gets no --before and the entry falls back to git.
BEFORE_FILE=""
_before_from_payload() {
    BEFORE_FILE="$(mktemp "${TMPDIR:-/tmp}/edit-integrity-before-XXXXXX")" || return 0
    if command -v jq >/dev/null 2>&1; then
        printf '%s' "$PAYLOAD" | jq -j '.tool_response.originalFile | strings' >"$BEFORE_FILE" 2>/dev/null || {
            rm -f "$BEFORE_FILE"
            BEFORE_FILE=""
            return 0
        }
    elif command -v python3 >/dev/null 2>&1; then
        printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    value = json.load(sys.stdin).get("tool_response", {}).get("originalFile")
    if isinstance(value, str):
        sys.stdout.write(value)
except Exception:
    pass
' >"$BEFORE_FILE" 2>/dev/null || {
            rm -f "$BEFORE_FILE"
            BEFORE_FILE=""
            return 0
        }
    else
        rm -f "$BEFORE_FILE"
        BEFORE_FILE=""
        return 0
    fi
    [ -s "$BEFORE_FILE" ] || {
        rm -f "$BEFORE_FILE"
        BEFORE_FILE=""
    }
}
if [ "$(printf '%s\n' "$TARGETS" | grep -c .)" = "1" ]; then
    _before_from_payload
fi

run_entry() {
    local candidate rc p
    local -a prefix=() paths=()
    [ -n "$BEFORE_FILE" ] && prefix=(--before "$BEFORE_FILE")
    while IFS= read -r p; do
        [ -n "$p" ] || continue
        paths+=("$p")
    done < <(printf '%s\n' "$TARGETS")
    for candidate in \
        "$(command -v fno-agents 2>/dev/null || true)" \
        "${FNO_AGENTS_BIN:-}" \
        "$PWD/crates/fno-agents/target/release/fno-agents" \
        "$PWD/crates/fno-agents/target/debug/fno-agents"; do
        [ -n "$candidate" ] || continue
        [ -x "$candidate" ] || continue
        "$candidate" hook edit-integrity \
            ${prefix[@]+"${prefix[@]}"} -- ${paths[@]+"${paths[@]}"}
        rc=$?
        [ "$rc" -le 1 ] && return 0
    done
    return 9
}

ENTRY_OUT="$(mktemp "${TMPDIR:-/tmp}/edit-integrity-out-XXXXXX")"
cleanup() {
    rm -f "$ENTRY_OUT"
    [ -n "$BEFORE_FILE" ] && rm -f "$BEFORE_FILE"
    exit 0
}
trap cleanup EXIT INT TERM

run_entry >"$ENTRY_OUT" 2>/dev/null
[ -s "$ENTRY_OUT" ] || exit 0
FINDINGS="$(cat "$ENTRY_OUT")"
[ -n "$FINDINGS" ] || exit 0

if command -v jq >/dev/null 2>&1; then
    CONTEXT="$(printf '%s' "$FINDINGS" | jq -Rs .)"
elif command -v python3 >/dev/null 2>&1; then
    CONTEXT="$(FINDINGS_ENV="$FINDINGS" python3 -c '
import json, os, sys
sys.stdout.write(json.dumps(os.environ["FINDINGS_ENV"]))
' 2>/dev/null || true)"
else
    CONTEXT=""
fi
[ -n "$CONTEXT" ] || exit 0

printf '{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":%s}}\n' "$CONTEXT"
exit 0
