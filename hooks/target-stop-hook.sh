#!/usr/bin/env bash
# hooks/target-stop-hook.sh -- control-plane collapse wedge (ab-d0337fbc), Task 2.1
#
# READ-ONLY SHIM: this file writes nothing to target-state.md.
# All stop/allow decision logic lives in crates/fno-agents/src/loopcheck.rs
# and is invoked via `fno-agents loop-check`.
#
# Binary resolution order (most-local wins to avoid the stale-installed-binary trap):
#   1. $FNO_AGENTS_BIN            (explicit env override)
#   2. <repo>/crates/fno-agents/target/release/fno-agents  (release build)
#   3. <repo>/crates/fno-agents/target/debug/fno-agents    (debug build)
#   4. $(command -v fno-agents)   (PATH fallback)
#
# ACTIVE-SESSION-AWARE error handling (x-81d9): the state file
# `.fno/target-state.md` is the active-session discriminator. With NO state file
# there is nothing to gate, so every failure path exits 0 (allow) as before.
# With a state file present, a broken checker (missing jq, missing/stale binary,
# verb non-zero, non-JSON output) is NOT a safe allow - silently allowing there
# disables the ship gate. Instead each such path bounded-blocks (exit 2) up to
# MAX_UNAVAIL_RETRIES so a transient breakage (mid-rebuild binary, flaky gh) can
# recover, then gives up LOUDLY (an event + exit 0) so a persistently-broken
# checker never wedges the session forever. The counter self-heals: it is
# removed on the first clean decision, so the bound is on CONSECUTIVE failures.
#
# Exit codes forwarded from the JSON decision:
#   0  allow  (includes all TerminationReason variants: DonePRGreen, NoWork, etc.)
#   2  block  (keep the session running; message echoed to stderr)

set -uo pipefail

# Consecutive checker-unavailable fires tolerated for an active session before a
# loud give-up allow. 3 gives a transient cause room to recover; 2-5 defensible
# (Claude's discretion). Named so the ceiling is obvious and tunable.
readonly MAX_UNAVAIL_RETRIES=3

# ── 1. Read stdin ─────────────────────────────────────────────────────────────
HOOK_INPUT=$(cat)

# ── 2. State file: the active-session discriminator ───────────────────────────
# No state file -> no target session here -> nothing to gate. This is the ONLY
# safe silent allow, and it gates every error path below: with a state file
# present, a checker that cannot do its job must block-and-signal, never allow.
STATE_FILE=".fno/target-state.md"
if [[ ! -f "$STATE_FILE" ]]; then
    exit 0
fi

# Active target session confirmed from here down.
REPO_ROOT=$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || echo "$PWD")
SESSION_ID=$(grep '^session_id:' "$STATE_FILE" 2>/dev/null \
    | head -1 | sed 's/^session_id:[[:space:]]*//' | tr -d '[:space:]' || true)

# Append an event to both project + global logs WITHOUT jq (this runs on the
# jq-missing give-up path too). Fields are hook-internal and safe to interpolate.
emit_event_both() {
    local etype="$1" data="$2" ts line
    ts=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)
    line="{\"ts\":\"${ts}\",\"type\":\"${etype}\",\"source\":\"hook\",\"data\":${data}}"
    mkdir -p "${REPO_ROOT}/.fno" "${HOME}/.fno" 2>/dev/null || true
    echo "$line" >> "${REPO_ROOT}/.fno/events.jsonl" 2>/dev/null || true
    echo "$line" >> "${HOME}/.fno/events.jsonl" 2>/dev/null || true
}

# Checker unavailable for an ACTIVE session: bounded-block, then loud give-up.
# Counter keyed by session_id so two sessions sharing a symlinked .fno never
# consume each other's retry budget (AC2-EDGE). Calls exit directly.
unavailable_block_or_allow() {
    local counter=".fno/.loop-check-unavail-${SESSION_ID}"
    local count=0
    [[ -f "$counter" ]] && count=$(tr -dc '0-9' < "$counter" 2>/dev/null)
    [[ -z "$count" ]] && count=0          # absent or corrupt -> start at 0
    count=$((10#$count + 1))              # 10# so a stray leading zero isn't read as octal
    echo "$count" > "$counter" 2>/dev/null || true
    if (( count <= MAX_UNAVAIL_RETRIES )); then
        echo "target stop-hook: checker unavailable (${count}/${MAX_UNAVAIL_RETRIES}), keeping session running" >&2
        exit 2
    fi
    emit_event_both "loop_check_unavailable_giveup" "{\"session_id\":\"${SESSION_ID}\",\"count\":${count}}"
    echo "target stop-hook: checker unavailable ${count} times; allowing stop (ship gate off for this stop)" >&2
    exit 0
}

# ── 3. jq required to parse the payload + decision ────────────────────────────
# Missing jq for an active session is checker-unavailable, not a safe allow.
if ! command -v jq >/dev/null 2>&1; then
    echo "target stop-hook: WARNING: jq not found for an active session" >&2
    unavailable_block_or_allow
fi

TRANSCRIPT_PATH=$(echo "$HOOK_INPUT" | jq -r '.transcript_path // empty' 2>/dev/null || true)

# An active target always has a transcript; its absence is an anomaly, not a
# reason to disable the gate.
if [[ -z "$TRANSCRIPT_PATH" ]] || [[ ! -f "$TRANSCRIPT_PATH" ]]; then
    echo "target stop-hook: WARNING: no transcript for an active session" >&2
    unavailable_block_or_allow
fi

# ── 4. Foreign-session guard (PR #388 fix class) ──────────────────────────────
# Extract the claude session id from state frontmatter. Read the current key
# (claude_session_id) first, falling back to the pre-x-2de3 key
# (the pre-rename claude_transcript_id) so an in-flight manifest written by an older binary
# still parses for one release.
MANIFEST_CTID=$(grep -E '^(claude_session_id|claude_transcript_id):' "$STATE_FILE" 2>/dev/null \
    | head -1 | sed -E 's/^(claude_session_id|claude_transcript_id):[[:space:]]*//' | tr -d '[:space:]')

# "null" = init ran without transcript-id env vars (diagnostic/non-Claude
# starts); treat it like empty so the guard never disables the hook (codex
# P2 on #447).
if [[ -n "$MANIFEST_CTID" && "$MANIFEST_CTID" != "null" ]]; then
    TRANSCRIPT_BASENAME=$(basename "$TRANSCRIPT_PATH" .jsonl)
    if [[ "$TRANSCRIPT_BASENAME" != "$MANIFEST_CTID" ]]; then
        # Another session's manifest; genuinely not ours to judge.
        exit 0
    fi
fi

# ── 5. Resolve the binary ─────────────────────────────────────────────────────
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

# ── 6. Binary missing for an active session: emit event + bounded-block ───────
# A stale/absent binary must not silently disable the ship gate; emit the
# diagnostic (as before) then route through the bounded-block helper.
if [[ -z "$BIN" ]]; then
    emit_event_both "loop_check_binary_missing" "{\"session_id\":\"${SESSION_ID}\"}"
    echo "target stop-hook: WARNING: fno-agents binary not found for an active session" >&2
    echo "target stop-hook: install with: cargo install --path crates/fno-agents --bins (needs a Rust toolchain/rustup)" >&2
    unavailable_block_or_allow
fi

# ── 7. Invoke the verb ────────────────────────────────────────────────────────
# The full hook payload rides the verb's stdin (--hook-input-stdin) so
# loop-check can read last_assistant_message - the stopping turn's final text,
# recomputed per fire - instead of racing the transcript flush (ab-223d2dae).
# stdin (not argv/env) because the message is unbounded and an oversized exec
# would fail into the shim's allow-exit path. A herestring (NOT a pipe!)
# because an OLD binary never reads stdin: with a pipe and a payload larger
# than the pipe buffer, the producer dies SIGPIPE(141), pipefail surfaces 141
# as the pipeline status, and the verb's real decision (possibly block) would
# be discarded into the allow-exit path - a fail-open. The herestring is
# materialized by bash before exec, so an old binary simply ignores its stdin
# and the captured exit code is the binary's own. (Trailing newline added by
# <<< is harmless: serde_json tolerates trailing whitespace.)
mkdir -p "${REPO_ROOT}/.fno" 2>/dev/null || true
LOOP_CHECK_LOG="${REPO_ROOT}/.fno/loop-check.stderr.log"
DECISION_JSON=""
verb_rc=0
DECISION_JSON=$("$BIN" loop-check \
    --state "$STATE_FILE" \
    --transcript "$TRANSCRIPT_PATH" \
    --cwd "$PWD" \
    --hook-input-stdin \
    2>>"$LOOP_CHECK_LOG" <<<"$HOOK_INPUT") || verb_rc=$?

if [[ $verb_rc -ne 0 ]]; then
    echo "target stop-hook: WARNING: fno-agents loop-check exited $verb_rc for an active session" >&2
    # Surface the verb's own stderr (the 2>> redirect above hides it from the
    # operator otherwise) - last lines name the actual cause.
    tail -n 5 "$LOOP_CHECK_LOG" >&2 2>/dev/null || true
    unavailable_block_or_allow
fi

if ! echo "$DECISION_JSON" | jq -e . >/dev/null 2>&1; then
    echo "target stop-hook: WARNING: fno-agents loop-check returned unexpected output (not JSON) for an active session" >&2
    unavailable_block_or_allow
fi

# ── 8. Clean decision reached: self-heal the unavailable counter ──────────────
# The checker worked. Reset the consecutive-failure counter FIRST so the bound
# is on consecutive failures only and a recovered checker starts fresh (AC2-FR).
rm -f ".fno/.loop-check-unavail-${SESSION_ID}" 2>/dev/null || true

# ── 9. Translate decision to hook protocol ────────────────────────────────────
DECISION=$(echo "$DECISION_JSON" | jq -r '.decision // "allow"')
MESSAGE=$(echo "$DECISION_JSON" | jq -r '.message // ""')

if [[ "$DECISION" == "block" ]]; then
    echo "target stop-hook: $MESSAGE" >&2
    exit 2
fi

# ── 10. Terminal-allow: invoke the finalize WRITER (step 6, ab-f8e5f214) ───────
# loop-check is the read-only DECISION; on a TERMINAL allow (a non-null
# termination_reason) the shim runs the separate `finalize` WRITER to re-home
# the ledger record + (ship-only) plan stamp/graduate + handoff artifact. This
# fires at the session-terminal boundary in EVERY mode (attended, autonomous,
# megawalk worker), so the records appear even when the agent compacted before
# the (now-removed) pre-promise side-effect bash.
#
# Strictly non-blocking: finalize is idempotent and best-effort. Any failure
# (old binary without the verb, missing python3, a sub-step error) is logged
# and ignored - side-effects NEVER change the completion decision. Run
# synchronously (NOT backgrounded): a backgrounded child would be SIGHUP'd when
# the session process exits, which defeats the survive-compaction goal.
TERMINATION_REASON=$(echo "$DECISION_JSON" | jq -r '.termination_reason // empty')
if [[ -n "$TERMINATION_REASON" ]]; then
    # Capture finalize's combined output: append the full trace to the log, and
    # ALSO surface it to the operator's stderr when finalize reports a sub-step
    # failure. finalize exits 0 even on a non-fatal sub-step failure (side-effects
    # never block), so the `|| ...` arm alone would miss those - the structured
    # session_finalize_failed EVENT is the canonical record, but an attended
    # operator should still see the failure without grepping events.jsonl.
    FINALIZE_OUT=""
    FINALIZE_RC=0
    FINALIZE_OUT="$("$BIN" finalize \
        --state "$STATE_FILE" \
        --transcript "$TRANSCRIPT_PATH" \
        --cwd "$PWD" \
        --reason "$TERMINATION_REASON" 2>&1)" || FINALIZE_RC=$?
    if [[ -n "$FINALIZE_OUT" ]]; then
        printf '%s\n' "$FINALIZE_OUT" >> "${REPO_ROOT}/.fno/finalize.stderr.log" 2>/dev/null || true
    fi
    if [[ $FINALIZE_RC -ne 0 ]] || printf '%s' "$FINALIZE_OUT" | grep -qi 'failed'; then
        echo "target stop-hook: finalize note (non-blocking): $(printf '%s' "$FINALIZE_OUT" | tail -n 3)" >&2
    fi
fi

# allow (includes DonePRGreen, DoneAdvisory, NoWork, Budget, NoProgress, etc.)
echo "target stop-hook: $MESSAGE" >&2
exit 0
