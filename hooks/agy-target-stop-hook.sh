#!/usr/bin/env bash
# hooks/agy-target-stop-hook.sh -- agy (Antigravity CLI) Stop-hook adapter.
#
# agy belongs in the CLAUDE native lane (its hooks are Claude-shaped event names:
# PreToolUse/PostToolUse/Stop/...), but its WIRE FORMAT is Gemini-family: camelCase
# stdin, a `decision:"continue"` keyword to KEEP WORKING (the inverse of Claude's
# exit-2/`block`), and JSON-decision-only stdout with NO exit-2 path. So this is a
# thin TRANSLATOR over the SAME completion authority Claude/OpenCode use
# (`fno-agents loop-check`), not a verbatim reuse of target-stop-hook.sh.
#
# Contract (agy `Stop` hook, "When agent tries to exit"):
#   stdin  (camelCase): conversationId, transcriptPath, workspacePaths[],
#                       artifactDirectoryPath, executionNum, terminationReason,
#                       error?, fullyIdle (bool: true only when all bg tasks done).
#   stdout: {"decision":"continue","reason":"<msg>"}  -> BLOCK the stop (keep working)
#           anything else (incl. {})                  -> ALLOW the stop
#   Silence rule: stdout must be ONLY the final JSON. All diagnostics go to stderr.
#   There is no exit-2 path for Stop; the script always exits 0 and the decision
#   is carried by the emitted JSON.
#
# Invariants:
#   - loop-check is the SOLE completion authority (no per-harness drift): this
#     adapter never decides "done" itself and never emits a fabricated termination.
#   - `fullyIdle == false` -> always continue; never allow a terminal stop while
#     async work is live.
#   - A repeated firing (executionNum increments) is safe: loop-check is idempotent.

set -uo pipefail

# ── emit the final decision JSON to stdout, then exit 0 (the ONLY stdout write) ─
emit() { printf '%s\n' "$1"; exit 0; }

# ── 1. Read stdin ─────────────────────────────────────────────────────────────
HOOK_INPUT=$(cat)

if ! command -v jq >/dev/null 2>&1; then
    # Can't parse the contract without jq. Allow the stop (an environment problem
    # must not trap the session in an unstoppable continue-loop). Not a fabricated
    # termination: allowing a stop emits no `termination` event; loop-check owns that.
    echo "agy stop-hook: WARNING: jq not found; allowing stop" >&2
    emit '{}'
fi

# Read fullyIdle BARE (not `// empty`): jq's `//` is a truthiness alternative, so
# `.fullyIdle // empty` collapses a meaningful boolean `false` to empty. Bare gives
# "true" / "false" / "null"(absent); only an explicit "false" blocks the stop.
FULLY_IDLE=$(printf '%s' "$HOOK_INPUT" | jq -r '.fullyIdle' 2>/dev/null || true)
TRANSCRIPT_PATH=$(printf '%s' "$HOOK_INPUT" | jq -r '.transcriptPath // empty' 2>/dev/null || true)
CONVERSATION_ID=$(printf '%s' "$HOOK_INPUT" | jq -r '.conversationId // empty' 2>/dev/null || true)

# ── 2. Background tasks still running -> never allow a terminal stop ───────────
# Only an EXPLICIT false blocks; a missing/empty field falls through to loop-check
# (the real authority), so a contract change can't wedge us into a forever-continue.
if [[ "$FULLY_IDLE" == "false" ]]; then
    emit '{"decision":"continue","reason":"background tasks still running"}'
fi

# ── 3. State file: no footnote session here -> allow the stop ──────────────────
# A plain native agy session (no manifest) is unaffected by this hook.
STATE_FILE=".fno/target-state.md"
if [[ ! -f "$STATE_FILE" ]]; then
    emit '{}'
fi

# ── 4. Synthesize a claude-shaped transcript loop-check can read ───────────────
# loop-check's scan filters lines on role=="assistant" (from /message/role OR
# top-level role) and reads text from /message/content (string | text-block array)
# OR top-level content. agy's transcript.jsonl uses Gemini-family conventions
# (likely role:"model" + parts:[{text}]), which that scan would skip. Normalize the
# realistic shapes to the line loop-check reads: {"message":{"role":"assistant",
# "content":"<text>"}}. fromjson? skips a malformed line individually (the scan is
# newest-first, so losing an early bad line is harmless).
#
# ponytail: agy's exact transcript.jsonl line schema is unconfirmed at build time
# (deferred capture, plan #jc 2026-07-11) -- this filter handles the documented-
# likely shapes (Gemini model/parts, claude message.content, flat role/content).
# A line matching none is dropped, which is SAFE: no promise detected -> the
# adapter keeps the session working (loop-check's world-gate + NoProgress backstop
# remain the real terminators). Tighten the filter once a real sample is captured.
synthesize_transcript() {
    jq -Rc '
        (fromjson? // empty) as $o
        | ($o.message.role // $o.role // "") as $r
        | select($r == "assistant" or $r == "model")
        | ($o.message.content // $o.content) as $c
        | (
            if   ($c | type) == "string" then $c
            elif ($c | type) == "array"  then ([ $c[] | (.text // empty) ] | join(" "))
            else ([ (($o.parts // $o.message.parts) // [])[] | (.text // empty) ] | join(" "))
            end
          ) as $t
        | select(($t | type) == "string" and ($t | length) > 0)
        | {message: {role: "assistant", content: $t}}
    ' "$1" 2>/dev/null
}

SYNTH="${STATE_FILE%/*}/.agy-loopcheck-${CONVERSATION_ID:-session}.jsonl"
mkdir -p ".fno" 2>/dev/null || true
if [[ -n "$TRANSCRIPT_PATH" && -f "$TRANSCRIPT_PATH" ]]; then
    synthesize_transcript "$TRANSCRIPT_PATH" > "$SYNTH" 2>/dev/null || : > "$SYNTH"
else
    # No transcript yet: an empty file -> loop-check finds no promise -> we keep
    # working (loop-check still runs its world-gate/backstop reads).
    : > "$SYNTH"
fi
# Clean up the synth file on every exit path (loop-check has read it by then).
trap 'rm -f "$SYNTH" 2>/dev/null || true' EXIT

# ── 5. Resolve the fno-agents binary (most-local wins; same order as the shim) ─
REPO_ROOT=$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || echo "$PWD")
BIN=""
if [[ -n "${FNO_AGENTS_BIN:-}" ]] && [[ -x "${FNO_AGENTS_BIN}" ]]; then
    BIN="$FNO_AGENTS_BIN"
elif [[ -x "${REPO_ROOT}/crates/fno-agents/target/release/fno-agents" ]]; then
    BIN="${REPO_ROOT}/crates/fno-agents/target/release/fno-agents"
elif [[ -x "${REPO_ROOT}/crates/fno-agents/target/debug/fno-agents" ]]; then
    BIN="${REPO_ROOT}/crates/fno-agents/target/debug/fno-agents"
elif command -v fno-agents >/dev/null 2>&1; then
    BIN=$(command -v fno-agents)
fi

emit_event() {
    # Append a hook event to both events.jsonl paths (best-effort, never fatal).
    local kind="$1"
    local sid ev
    sid=$(grep '^session_id:' "$STATE_FILE" 2>/dev/null | head -1 \
        | sed 's/^session_id:[[:space:]]*//' | tr -d '[:space:]' || true)
    ev=$(jq -nc \
        --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        --arg sid "$sid" \
        --arg kind "$kind" \
        '{ts:$ts,type:$kind,source:"hook",data:{session_id:$sid,harness:"agy"}}' 2>/dev/null || true)
    [[ -z "$ev" ]] && return 0
    mkdir -p ".fno" "${HOME}/.fno" 2>/dev/null || true
    printf '%s\n' "$ev" >> ".fno/events.jsonl" 2>/dev/null || true
    printf '%s\n' "$ev" >> "${HOME}/.fno/events.jsonl" 2>/dev/null || true
}

# ── 6. Binary genuinely MISSING -> allow stop (don't trap an unstoppable loop) ─
# A never-installed binary can't be fixed by retrying within the session, so a
# forever-continue would be worse than letting agy exit. Allowing a stop is NOT a
# fabricated termination (no `termination` event written). A TRANSIENT failure of a
# present binary is handled separately below (continue + retry next fire).
if [[ -z "$BIN" ]]; then
    emit_event "loop_check_binary_missing"
    echo "agy stop-hook: WARNING: fno-agents binary not found; allowing stop" >&2
    echo "agy stop-hook: install with: cargo install --path crates/fno-agents --bins" >&2
    emit '{}'
fi

# ── 7. Invoke loop-check (transcript scan only; agy stdin has no last message) ─
DECISION_JSON=""
verb_rc=0
DECISION_JSON=$("$BIN" loop-check \
    --state "$STATE_FILE" \
    --transcript "$SYNTH" \
    --cwd "$PWD" \
    2>>".fno/agy-loop-check.stderr.log") || verb_rc=$?

# ── 8. Transient failure of a present binary -> continue (retry next fire) ────
# The plan's rule: when loop-check can't return a verdict, do NOT fabricate a
# termination -- keep working and let the next firing retry. loop-check is the sole
# authority, so we never allow a stop on its say-so when it didn't actually speak.
if [[ $verb_rc -ne 0 ]] || ! printf '%s' "$DECISION_JSON" | jq -e . >/dev/null 2>&1; then
    emit_event "loop_check_gh_error"
    echo "agy stop-hook: WARNING: loop-check unavailable (rc=$verb_rc / non-JSON); continuing" >&2
    tail -n 5 ".fno/agy-loop-check.stderr.log" >&2 2>/dev/null || true
    emit '{"decision":"continue","reason":"loop-check unavailable; will retry on the next stop"}'
fi

# ── 9. Translate the verdict to the agy decision protocol ─────────────────────
DECISION=$(printf '%s' "$DECISION_JSON" | jq -r '.decision // "allow"')
MESSAGE=$(printf '%s' "$DECISION_JSON" | jq -r '.message // ""')
TERMINATION_REASON=$(printf '%s' "$DECISION_JSON" | jq -r '.termination_reason // empty')

if [[ "$DECISION" == "block" ]]; then
    # The world has not caught up (or there's no promise yet): keep working.
    echo "agy stop-hook: $MESSAGE" >&2
    emit "$(jq -nc --arg r "$MESSAGE" '{decision:"continue",reason:$r}')"
fi

# ── 10. Terminal allow -> run the finalize WRITER (ledger + ship stamp), then stop
# Mirrors the claude shim: loop-check is the read-only decision; finalize is the
# idempotent, best-effort side-effect writer. Any failure is logged and ignored --
# side-effects never change the completion decision. Run synchronously (a
# backgrounded child would be SIGHUP'd when agy exits).
if [[ -n "$TERMINATION_REASON" ]]; then
    FINALIZE_OUT=""
    FINALIZE_OUT="$("$BIN" finalize \
        --state "$STATE_FILE" \
        --transcript "$SYNTH" \
        --cwd "$PWD" \
        --reason "$TERMINATION_REASON" 2>&1)" || true
    [[ -n "$FINALIZE_OUT" ]] && printf '%s\n' "$FINALIZE_OUT" >> ".fno/finalize.stderr.log" 2>/dev/null || true
fi

# Allow the stop. loop-check already emitted its own `termination` event on a
# terminal allow, so we add nothing to stdout but the allow signal.
echo "agy stop-hook: $MESSAGE" >&2
emit '{}'
