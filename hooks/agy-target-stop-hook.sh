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
#
# ACTIVE-SESSION-AWARE error handling (x-984e, porting x-81d9): the state file
# `.fno/target-state.md` is the active-session discriminator, checked BEFORE any
# error path. With NO state file there is nothing to gate, so every failure emits
# `{}` (allow) as before. With a state file present, a broken checker (missing jq,
# missing binary, verb non-zero, non-JSON output) is NOT a safe fail-open allow --
# silently allowing there disables the ship gate. Instead each such path
# bounded-CONTINUES up to MAX_UNAVAIL_RETRIES so a transient breakage (mid-rebuild
# binary, flaky gh) can recover, then gives up LOUDLY (an event + `{}`) so a
# persistently-broken checker never wedges the session into a forever-continue.
# The counter self-heals: it is removed on the first clean decision.

set -uo pipefail

# Consecutive checker-unavailable fires tolerated for an active session before a
# loud give-up allow (mirrors target-stop-hook.sh's MAX_UNAVAIL_RETRIES).
readonly MAX_UNAVAIL_RETRIES=3

# ── emit the final decision JSON to stdout, then exit 0 (the ONLY stdout write) ─
emit() { printf '%s\n' "$1"; exit 0; }

# ── 1. Read stdin ─────────────────────────────────────────────────────────────
HOOK_INPUT=$(cat)
HAVE_JQ=1
command -v jq >/dev/null 2>&1 || HAVE_JQ=0

# Resolve the WORKSPACE ROOT, not $PWD. agy can fire Stop from a subdirectory, or
# via the global ~/.gemini/config/hooks.json with an unrelated cwd; a relative
# .fno/target-state.md lookup would then miss the manifest init writes at the git
# root and stop an active target. agy's stdin carries workspacePaths[]; read it
# with jq when present, else a jq-free sed extraction of the first entry -- so a
# jq-missing hook firing from an UNRELATED cwd still locates the active manifest
# instead of falling to $PWD, missing it, and failing OPEN on the very gate this
# hook exists to hold (peer review, PR #313). Fall back to git toplevel then $PWD.
# Every downstream path (state, synth, logs, events, --cwd) hangs off ROOT.
WORKSPACE_ROOT=""
[[ $HAVE_JQ -eq 1 ]] && WORKSPACE_ROOT=$(printf '%s' "$HOOK_INPUT" | jq -r '.workspacePaths[0] // empty' 2>/dev/null || true)
if [[ -z "$WORKSPACE_ROOT" ]]; then
    # jq-free fallback: first string in a simple "workspacePaths":["..."] array.
    # A miss (absent field, empty array) leaves it empty -> git-toplevel/$PWD.
    WORKSPACE_ROOT=$(printf '%s' "$HOOK_INPUT" \
        | sed -n 's/.*"workspacePaths"[[:space:]]*:[[:space:]]*\[[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
fi
if [[ -n "$WORKSPACE_ROOT" && -d "$WORKSPACE_ROOT" ]]; then
    ROOT="$WORKSPACE_ROOT"
else
    ROOT=$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || echo "$PWD")
fi
STATE_FILE="$ROOT/.fno/target-state.md"

# conversationId (agy's session discriminator, jq-only) -- read where jq exists;
# the counter helper falls back to the state-file session_id when it is empty.
CONVERSATION_ID=""

# jq-free event writer (string interpolation, so it also runs on the jq-missing
# give-up path). Fields are hook-internal and safe to interpolate.
emit_event() {
    local kind="$1" sid ts line
    sid=$(grep '^session_id:' "$STATE_FILE" 2>/dev/null | head -1 \
        | sed 's/^session_id:[[:space:]]*//' | tr -d '[:space:]' || true)
    ts=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || true)
    line="{\"ts\":\"${ts}\",\"type\":\"${kind}\",\"source\":\"hook\",\"data\":{\"session_id\":\"${sid}\",\"harness\":\"agy\"}}"
    # ${HOME:-} (not ${HOME}): under set -u an unset HOME would abort the script.
    mkdir -p "$ROOT/.fno" "${HOME:-}/.fno" 2>/dev/null || true
    printf '%s\n' "$line" >> "$ROOT/.fno/events.jsonl" 2>/dev/null || true
    printf '%s\n' "$line" >> "${HOME:-}/.fno/events.jsonl" 2>/dev/null || true
}

# Counter path keyed by conversationId (else the state-file session_id) so two
# agy sessions sharing a symlinked .fno never consume each other's retry budget.
counter_path() {
    local key="$CONVERSATION_ID"
    [[ -z "$key" ]] && key=$(grep '^session_id:' "$STATE_FILE" 2>/dev/null | head -1 \
        | sed 's/^session_id:[[:space:]]*//' | tr -d '[:space:]' || true)
    [[ -z "$key" ]] && key="session"
    printf '%s' "$ROOT/.fno/.loop-check-unavail-${key}"
}

# Checker unavailable for an ACTIVE session: bounded-continue, then loud give-up
# allow. Emits via agy's JSON decision protocol (not exit-2). Calls emit directly.
unavailable_continue_or_allow() {
    local counter count=0
    counter=$(counter_path)
    [[ -f "$counter" ]] && count=$(tr -dc '0-9' < "$counter" 2>/dev/null || true)
    [[ -z "$count" ]] && count=0          # absent or corrupt -> start at 0
    count=$((10#$count + 1))              # 10# so a stray leading zero isn't octal
    echo "$count" > "$counter" 2>/dev/null || true
    if (( count <= MAX_UNAVAIL_RETRIES )); then
        echo "agy stop-hook: checker unavailable (${count}/${MAX_UNAVAIL_RETRIES}), keeping session running" >&2
        emit "$(printf '{"decision":"continue","reason":"checker unavailable (%s/%s); will retry on the next stop"}' "$count" "$MAX_UNAVAIL_RETRIES")"
    fi
    emit_event "loop_check_unavailable_giveup"
    echo "agy stop-hook: checker unavailable ${count} times; allowing stop (ship gate off for this stop)" >&2
    emit '{}'
}

# ── 2. Background tasks still running -> never allow a terminal stop ───────────
# fullyIdle is jq-only; read it BARE (not `// empty`): jq's `//` is a truthiness
# alternative, so `.fullyIdle // empty` collapses a meaningful boolean `false` to
# empty. Only an EXPLICIT "false" blocks; a missing/empty field falls through.
if [[ $HAVE_JQ -eq 1 ]]; then
    FULLY_IDLE=$(printf '%s' "$HOOK_INPUT" | jq -r '.fullyIdle' 2>/dev/null || true)
    if [[ "$FULLY_IDLE" == "false" ]]; then
        emit '{"decision":"continue","reason":"background tasks still running"}'
    fi
fi

# ── 3. State file: the active-session discriminator (gates every error path) ───
# No manifest -> no footnote session here -> allow the stop. This precedes the
# jq-missing and binary-missing error paths so they bounded-continue ONLY for an
# active session; with no state file every failure still emits `{}` (allow).
if [[ ! -f "$STATE_FILE" ]]; then
    emit '{}'
fi

# ── 3a. jq missing for an ACTIVE session: checker-unavailable, not a safe allow ─
if [[ $HAVE_JQ -eq 0 ]]; then
    echo "agy stop-hook: WARNING: jq not found for an active session" >&2
    unavailable_continue_or_allow
fi

# jq confirmed present from here down.
CONVERSATION_ID=$(printf '%s' "$HOOK_INPUT" | jq -r '.conversationId // empty' 2>/dev/null || true)
TRANSCRIPT_PATH=$(printf '%s' "$HOOK_INPUT" | jq -r '.transcriptPath // empty' 2>/dev/null || true)

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
mkdir -p "$ROOT/.fno" 2>/dev/null || true
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
REPO_ROOT=$(git -C "$ROOT" rev-parse --show-toplevel 2>/dev/null || echo "$ROOT")
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

# ── 6. Binary MISSING for an ACTIVE session: emit event + bounded-continue ─────
# A stale/absent binary must not silently disable the ship gate. Emit the
# diagnostic (as before) then route through the bounded-continue helper so a
# transient absence (mid-rebuild) recovers, and a persistent one gives up loudly
# rather than trapping the session in a forever-continue.
if [[ -z "$BIN" ]]; then
    emit_event "loop_check_binary_missing"
    echo "agy stop-hook: WARNING: fno-agents binary not found for an active session" >&2
    echo "agy stop-hook: install with: cargo install --path crates/fno-agents --bins" >&2
    unavailable_continue_or_allow
fi

# ── 7. Invoke loop-check (transcript scan only; agy stdin has no last message) ─
DECISION_JSON=""
verb_rc=0
DECISION_JSON=$("$BIN" loop-check \
    --state "$STATE_FILE" \
    --transcript "$SYNTH" \
    --cwd "$ROOT" \
    2>>"$ROOT/.fno/agy-loop-check.stderr.log") || verb_rc=$?

# ── 8. Transient failure of a present binary -> bounded-continue (retry next) ──
# When loop-check can't return a verdict, do NOT fabricate a termination -- keep
# working and let the next firing retry. Routed through the SAME bounded counter
# as the missing-checker paths so a PERMANENTLY-broken checker gives up loudly
# instead of wedging the session into a forever-continue.
if [[ $verb_rc -ne 0 ]] || ! printf '%s' "$DECISION_JSON" | jq -e . >/dev/null 2>&1; then
    emit_event "loop_check_gh_error"
    echo "agy stop-hook: WARNING: loop-check unavailable (rc=$verb_rc / non-JSON)" >&2
    tail -n 5 "$ROOT/.fno/agy-loop-check.stderr.log" >&2 2>/dev/null || true
    unavailable_continue_or_allow
fi

# ── 8a. Clean decision reached: self-heal the unavailable counter ─────────────
# The checker worked. Reset the consecutive-failure counter so the bound is on
# CONSECUTIVE failures only and a recovered checker starts fresh.
rm -f "$(counter_path)" 2>/dev/null || true

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
        --cwd "$ROOT" \
        --reason "$TERMINATION_REASON" 2>&1)" || true
    [[ -n "$FINALIZE_OUT" ]] && printf '%s\n' "$FINALIZE_OUT" >> "$ROOT/.fno/finalize.stderr.log" 2>/dev/null || true
fi

# Allow the stop. loop-check already emitted its own `termination` event on a
# terminal allow, so we add nothing to stdout but the allow signal.
echo "agy stop-hook: $MESSAGE" >&2
emit '{}'
