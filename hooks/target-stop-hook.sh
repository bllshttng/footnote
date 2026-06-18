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
# On binary missing: emits loop_check_binary_missing to both events.jsonl paths
# and exits 0 (allow). An unstoppable block-loop with no checker is worse than
# letting the session continue; CI catches regressions independently.
#
# Exit codes forwarded from the JSON decision:
#   0  allow  (includes all TerminationReason variants: DonePRGreen, NoWork, etc.)
#   2  block  (keep the session running; message echoed to stderr)

set -uo pipefail

# ── 1. Read stdin ─────────────────────────────────────────────────────────────
HOOK_INPUT=$(cat)

if ! command -v jq >/dev/null 2>&1; then
    echo "target stop-hook: WARNING: jq not found; allowing session to continue" >&2
    exit 0
fi

TRANSCRIPT_PATH=$(echo "$HOOK_INPUT" | jq -r '.transcript_path // empty' 2>/dev/null || true)

if [[ -z "$TRANSCRIPT_PATH" ]] || [[ ! -f "$TRANSCRIPT_PATH" ]]; then
    exit 0
fi

# ── 2. State file ─────────────────────────────────────────────────────────────
STATE_FILE=".fno/target-state.md"
if [[ ! -f "$STATE_FILE" ]]; then
    exit 0
fi

# ── 3. Foreign-session guard (PR #388 fix class) ──────────────────────────────
# Extract claude_transcript_id from state frontmatter.
MANIFEST_CTID=$(grep '^claude_transcript_id:' "$STATE_FILE" 2>/dev/null \
    | head -1 | sed 's/^claude_transcript_id:[[:space:]]*//' | tr -d '[:space:]')

# "null" = init ran without transcript-id env vars (diagnostic/non-Claude
# starts); treat it like empty so the guard never disables the hook (codex
# P2 on #447).
if [[ -n "$MANIFEST_CTID" && "$MANIFEST_CTID" != "null" ]]; then
    TRANSCRIPT_BASENAME=$(basename "$TRANSCRIPT_PATH" .jsonl)
    if [[ "$TRANSCRIPT_BASENAME" != "$MANIFEST_CTID" ]]; then
        # Another session's manifest; not ours to judge.
        exit 0
    fi
fi

# ── 4. Resolve the binary ─────────────────────────────────────────────────────
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

# ── 5. Binary missing: emit event + allow ─────────────────────────────────────
if [[ -z "$BIN" ]]; then
    SESSION_ID=$(grep '^session_id:' "$STATE_FILE" 2>/dev/null \
        | head -1 | sed 's/^session_id:[[:space:]]*//' | tr -d '[:space:]' || true)
    EVENT=$(jq -nc \
        --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u +%Y-%m-%dT%H:%M:%SZ)" \
        --arg sid "$SESSION_ID" \
        '{ts:$ts,type:"loop_check_binary_missing",source:"hook",data:{session_id:$sid}}')
    mkdir -p ".fno" "${HOME}/.fno" 2>/dev/null || true
    echo "$EVENT" >> ".fno/events.jsonl" 2>/dev/null || true
    echo "$EVENT" >> "${HOME}/.fno/events.jsonl" 2>/dev/null || true
    echo "target stop-hook: WARNING: fno-agents binary not found; allowing session to continue" >&2
    echo "target stop-hook: install with: cargo install --path crates/fno-agents --bins" >&2
    exit 0
fi

# ── 6. Invoke the verb ────────────────────────────────────────────────────────
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
mkdir -p ".fno" 2>/dev/null || true
DECISION_JSON=""
verb_rc=0
DECISION_JSON=$("$BIN" loop-check \
    --state "$STATE_FILE" \
    --transcript "$TRANSCRIPT_PATH" \
    --cwd "$PWD" \
    --hook-input-stdin \
    2>>".fno/loop-check.stderr.log" <<<"$HOOK_INPUT") || verb_rc=$?

if [[ $verb_rc -ne 0 ]]; then
    echo "target stop-hook: WARNING: fno-agents loop-check exited $verb_rc; allowing session to continue" >&2
    # Surface the verb's own stderr (the 2>> redirect above hides it from the
    # operator otherwise) - last lines name the actual cause.
    tail -n 5 ".fno/loop-check.stderr.log" >&2 2>/dev/null || true
    exit 0
fi

if ! echo "$DECISION_JSON" | jq -e . >/dev/null 2>&1; then
    echo "target stop-hook: WARNING: fno-agents loop-check returned unexpected output (not JSON); allowing session to continue" >&2
    exit 0
fi

# ── 7. Translate decision to hook protocol ────────────────────────────────────
DECISION=$(echo "$DECISION_JSON" | jq -r '.decision // "allow"')
MESSAGE=$(echo "$DECISION_JSON" | jq -r '.message // ""')

if [[ "$DECISION" == "block" ]]; then
    echo "target stop-hook: $MESSAGE" >&2
    exit 2
fi

# ── 8. Terminal-allow: invoke the finalize WRITER (step 6, ab-f8e5f214) ────────
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
        printf '%s\n' "$FINALIZE_OUT" >> ".fno/finalize.stderr.log" 2>/dev/null || true
    fi
    if [[ $FINALIZE_RC -ne 0 ]] || printf '%s' "$FINALIZE_OUT" | grep -qi 'failed'; then
        echo "target stop-hook: finalize note (non-blocking): $(printf '%s' "$FINALIZE_OUT" | tail -n 3)" >&2
    fi
fi

# allow (includes DonePRGreen, DoneAdvisory, NoWork, Budget, NoProgress, etc.)
echo "target stop-hook: $MESSAGE" >&2
exit 0
