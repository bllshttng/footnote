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
# The session manifest (the worktree slice of the repo's space; legacy
# `<repo>/.fno/target-state.md`) is the active-session discriminator. With NO state file
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
#   0  block on the claude harness (stdout {"decision":"block",...}; see below)
#   2  block  (keep the session running; message echoed to stderr)
#
# On the claude harness a block is instead emitted as stdout JSON
# {"decision":"block","reason":...} at exit 0: Claude Code's structured Stop
# decision. Claude Code ignores JSON on a non-zero exit and renders exit-2 +
# stderr as a "Stop hook error", while codex/gemini read only the exit code -
# so exit 2 remains their block signal.

set -uo pipefail

# Emit a block the way the CALLING harness honors it, then exit. Claude Code
# honors a Stop block only as structured stdout JSON at exit 0; an exit-2 block
# renders as a "Stop hook error" and the session stops anyway. Codex/gemini read
# only the exit code, so exit 2 stays their block signal. The claude markers are
# claims, not proof (see the king resolution below): a non-claude session
# spawned under a claude parent inherits them, and reading them as claude hands
# that session a block it parses as allow. A foreign marker in the same env
# makes the claim ambiguous, so the legacy exit-2 path runs instead.
emit_block_for_harness() {
    local reason="$1" json=""
    if [[ "${CLAUDECODE:-0}" == "1" || -n "${CLAUDE_PLUGIN_ROOT:-}" ]] \
        && [[ -z "${CODEX_THREAD_ID:-}" && -z "${CODEX_SESSION_ID:-}" \
            && -z "${GEMINI_SESSION_ID:-}" && -z "${OPENCODE_SESSION_ID:-}" ]] \
        && json=$(jq -cn --arg r "$reason" '{"decision":"block","reason":$r}' 2>/dev/null) \
        && [[ -n "$json" ]]; then
        printf '%s\n' "$json"
        exit 0
    fi
    echo "target stop-hook: $reason" >&2
    exit 2
}

# Consecutive checker-unavailable fires tolerated for an active session before a
# loud give-up allow. 3 gives a transient cause room to recover; 2-5 defensible
# (Claude's discretion). Named so the ceiling is obvious and tunable.
readonly MAX_UNAVAIL_RETRIES=3

# ── 1. Read stdin ─────────────────────────────────────────────────────────────
HOOK_INPUT=$(cat)
HOOK_TRANSCRIPT_PATH=$(printf '%s' "$HOOK_INPUT" | sed -n \
    's/.*"transcript_path"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
HOOK_HARNESS_ID=$(basename "$HOOK_TRANSCRIPT_PATH" .jsonl 2>/dev/null || true)
# Every harness sends its own session id in the payload, and it is the ONLY
# source that is already the bare id the resolver wants. Codex names its
# transcript rollout-<utc>-<thread-uuid>, so the basename needs stripping, and
# `transcript_path` is nullable in codex's own stop.command.input schema, so the
# basename can be absent entirely. Deriving the id from the path alone worked
# only when CODEX_THREAD_ID happened to be exported to supply the answer, and
# the hook runner calls env_clear() and replays the session snapshot plus the
# hook's declared env (fno declares none), so in production it is not there.
# Measured 2026-09-02: the resolver got the prefixed basename, missed, and the
# hook took the silent-allow path below - 1666 claude loop-check events against
# 2 codex over six days, while other Stop hooks in the same group fired fine.
HOOK_SESSION_ID=$(printf '%s' "$HOOK_INPUT" | sed -n \
    's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
# The transcript basename stays the TRANSCRIPT identity that the ownership
# guards below compare against; only the resolver argument changes here. When
# the payload carries no path, the session id is the only identity there is.
[[ -n "$HOOK_HARNESS_ID" ]] || HOOK_HARNESS_ID="$HOOK_SESSION_ID"

# `manifest-for-session` is strict trimmed equality against one of three stamped
# fields (crates/fno-agents/src/manifest_lookup.rs `matches`): no suffix match,
# no rollout stripping. So a single guessed id is one shot, and a miss is
# indistinguishable from "no manifest names this session" - the silent allow.
# Collect every id this stop could legitimately be known by instead, most
# authoritative first, and let the resolver say which one the manifest stamped.
CODEX_UUID=$(printf '%s' "$HOOK_HARNESS_ID" | sed -E \
    's/^.*-([0-9a-fA-F]{8}(-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12})$/\1/' 2>/dev/null || true)
[[ "$CODEX_UUID" != "$HOOK_HARNESS_ID" ]] || CODEX_UUID=""
# CODEX_THREAD_ID stays gated on matching this transcript. It is an inherited
# env marker, so a session spawned under a codex parent carries the PARENT's
# thread id; trusting it unconditionally would hand the resolver a foreign
# session and narrow the king lookup onto the wrong harness. The payload id and
# the shape-stripped uuid are both statements about THIS stop, so they need no
# such guard.
CODEX_ENV_ID=""
if [[ -n "${CODEX_THREAD_ID:-}" ]] \
    && { [[ "$HOOK_HARNESS_ID" == "$CODEX_THREAD_ID" ]] \
        || [[ "$HOOK_HARNESS_ID" == *"-$CODEX_THREAD_ID" ]]; }; then
    CODEX_ENV_ID="$CODEX_THREAD_ID"
fi
RESOLVE_IDS=()
for candidate in "$HOOK_SESSION_ID" "$CODEX_ENV_ID" "$CODEX_UUID" "$HOOK_HARNESS_ID"; do
    [[ -n "$candidate" ]] || continue
    for seen in ${RESOLVE_IDS[@]+"${RESOLVE_IDS[@]}"}; do
        [[ "$seen" == "$candidate" ]] && continue 2
    done
    RESOLVE_IDS+=("$candidate")
done
RESOLVE_HARNESS_ID="${RESOLVE_IDS[0]:-}"

# Try each candidate; echo the manifest path and return 0 on the first hit.
# Returns 1 only when EVERY candidate cleanly missed (a real stranger), and 2
# when the resolver could not answer - the caller keeps those apart because a
# clean miss lets the session go while a broken resolver must block.
resolve_manifest_state() {
    local bin="$1" id state rc worst=1
    shift
    for id in "$@"; do
        rc=0
        state=$("$bin" manifest-for-session --harness-session-id "$id" 2>/dev/null) || rc=$?
        if [[ "$rc" -eq 0 && -n "$state" ]]; then
            printf '%s' "$state"
            return 0
        fi
        [[ "$rc" -eq 1 ]] || worst=2
    done
    return "$worst"
}

# ── 2. State file: the active-session discriminator ───────────────────────────
# No state file -> no target session here -> nothing to gate. This is the ONLY
# safe silent allow, and it gates every error path below: with a state file
# present, a checker that cannot do its job must block-and-signal, never allow.
# The manifest lives in the worktree slice of the repo's space; resolve it
# through the owning verb so this hook never spells the path. Degraded
# fallback for an fno predating the verb: the legacy checkout-relative path.
LIVE_STATE_FILE=$(fno-agents state path target-state 2>/dev/null || true)
[[ -z "$LIVE_STATE_FILE" ]] && LIVE_STATE_FILE=".fno/target-state.md"
STATE_FILE="$LIVE_STATE_FILE"
TARGET_CWD="$PWD"
REPO_ROOT=$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || echo "$PWD")
WORKTREE_COUNT=$(git -C "$PWD" worktree list --porcelain 2>/dev/null \
    | grep -c '^worktree ' || true)
[[ "$WORKTREE_COUNT" =~ ^[0-9]+$ ]] || WORKTREE_COUNT=0
OTHER_WORKTREE_PRESENT=0
(( WORKTREE_COUNT > 1 )) && OTHER_WORKTREE_PRESENT=1
# The project space root: cross-worktree journals, kings and stop-hook
# diagnostics live here, never in the checkout.
SPACE_DIR=$(dirname "$(fno-agents state path events 2>/dev/null || true)")
[[ -z "$SPACE_DIR" || "$SPACE_DIR" == "." ]] && SPACE_DIR="${REPO_ROOT}/.fno"

resolve_agents_bin() {
    if [[ -n "${FNO_AGENTS_BIN:-}" ]] && [[ -x "${FNO_AGENTS_BIN}" ]]; then
        printf '%s' "$FNO_AGENTS_BIN"
    elif [[ -x "${REPO_ROOT}/crates/fno-agents/target/release/fno-agents" ]]; then
        printf '%s' "${REPO_ROOT}/crates/fno-agents/target/release/fno-agents"
    elif [[ -x "${REPO_ROOT}/crates/fno-agents/target/debug/fno-agents" ]]; then
        printf '%s' "${REPO_ROOT}/crates/fno-agents/target/debug/fno-agents"
    elif command -v fno-agents >/dev/null 2>&1; then
        command -v fno-agents
    fi
}

BIN=""
TARGET_RESOLVE_BROKEN=0
TARGET_NO_MATCH=0
if [[ -f "$LIVE_STATE_FILE" ]]; then
    RESIDENT_SESSION_ID=$(sed -n 's/^fno_id:[[:space:]]*//p' "$LIVE_STATE_FILE" 2>/dev/null \
        | head -1 | tr -d '[:space:]' || true)
    [[ -n "$RESIDENT_SESSION_ID" ]] || RESIDENT_SESSION_ID=$(sed -n \
        's/^session_id:[[:space:]]*//p' "$LIVE_STATE_FILE" 2>/dev/null \
        | head -1 | tr -d '[:space:]' || true)
    RESIDENT_HARNESS_ID=$(grep -E '^(harness_session_id|claude_session_id|claude_transcript_id):' \
        "$LIVE_STATE_FILE" 2>/dev/null \
        | sed -E 's/^(harness_session_id|claude_session_id|claude_transcript_id):[[:space:]]*//' \
        | grep -Ev '^(null)?$' | head -1 | tr -d '[:space:]' || true)
    RESIDENT_MATCH=0
    if [[ -n "$HOOK_HARNESS_ID" && -n "$RESIDENT_HARNESS_ID" ]] \
        && { [[ "$RESIDENT_HARNESS_ID" == "$HOOK_HARNESS_ID" ]] \
            || [[ "$HOOK_HARNESS_ID" == *"-$RESIDENT_HARNESS_ID" ]]; }; then
        RESIDENT_MATCH=1
    elif [[ -n "$HOOK_HARNESS_ID" && -z "$RESIDENT_HARNESS_ID" \
        && "$RESIDENT_SESSION_ID" == "$HOOK_HARNESS_ID" ]]; then
        RESIDENT_MATCH=1
    fi
    if [[ "$RESIDENT_MATCH" -ne 1 ]]; then
        BIN=$(resolve_agents_bin)
        RESOLVED_STATE=""
        RESOLVE_RC=2
        if [[ -n "$BIN" && ${#RESOLVE_IDS[@]} -gt 0 ]]; then
            RESOLVE_RC=0
            RESOLVED_STATE=$(resolve_manifest_state "$BIN" "${RESOLVE_IDS[@]}") || RESOLVE_RC=$?
        fi
        RESOLVED_CWD=""
        if [[ "$RESOLVE_RC" -eq 0 && -n "$RESOLVED_STATE" && -f "$RESOLVED_STATE" ]]; then
            RESOLVED_CWD=$(cd "$(dirname "$RESOLVED_STATE")/.." 2>/dev/null && pwd -P) || true
        fi
        if [[ -n "$RESOLVED_CWD" ]]; then
            LIVE_STATE_FILE="$RESOLVED_STATE"
            STATE_FILE="$RESOLVED_STATE"
            TARGET_CWD="$RESOLVED_CWD"
        elif [[ "$RESOLVE_RC" -eq 1 ]]; then
            TARGET_NO_MATCH=1
            LIVE_STATE_FILE=""
            STATE_FILE=""
        else
            TARGET_RESOLVE_BROKEN=1
        fi
    fi
else
    BIN=$(resolve_agents_bin)
    if [[ -n "$BIN" && -n "$RESOLVE_HARNESS_ID" ]]; then
        RESOLVE_RC=0
        RESOLVED_STATE=$(resolve_manifest_state "$BIN" "${RESOLVE_IDS[@]}") || RESOLVE_RC=$?
        RESOLVED_CWD=""
        if [[ "$RESOLVE_RC" -eq 0 && -n "$RESOLVED_STATE" && -f "$RESOLVED_STATE" ]]; then
            RESOLVED_CWD=$(cd "$(dirname "$RESOLVED_STATE")/.." 2>/dev/null && pwd -P) || true
        fi
        if [[ -n "$RESOLVED_CWD" ]]; then
            LIVE_STATE_FILE="$RESOLVED_STATE"
            STATE_FILE="$RESOLVED_STATE"
            TARGET_CWD="$RESOLVED_CWD"
        elif [[ "$RESOLVE_RC" -eq 1 ]]; then
            TARGET_NO_MATCH=1
        elif [[ "$OTHER_WORKTREE_PRESENT" -eq 1 ]]; then
            TARGET_RESOLVE_BROKEN=1
        fi
    elif [[ "$OTHER_WORKTREE_PRESENT" -eq 1 ]]; then
        TARGET_RESOLVE_BROKEN=1
    fi
fi

DELIVERY_PENDING_PREFIX=$(git -C "$REPO_ROOT" rev-parse --git-path fno-delivery-finalize-pending- 2>/dev/null \
    || printf '%s' "${REPO_ROOT}/.fno/.delivery-finalize-pending-")
case "$DELIVERY_PENDING_PREFIX" in
    /*) ;;
    *) DELIVERY_PENDING_PREFIX="${REPO_ROOT}/${DELIVERY_PENDING_PREFIX}" ;;
esac
if [[ -f "$LIVE_STATE_FILE" ]]; then
    LIVE_SESSION_ID=$(sed -n 's/^fno_id:[[:space:]]*//p' "$LIVE_STATE_FILE" 2>/dev/null \
        | head -1 | tr -d '[:space:]' || true)
    if [[ -z "$LIVE_SESSION_ID" ]]; then
        LIVE_SESSION_ID=$(sed -n 's/^session_id:[[:space:]]*//p' "$LIVE_STATE_FILE" 2>/dev/null \
            | head -1 | tr -d '[:space:]' || true)
    fi
    LIVE_HARNESS_ID=$(grep -E '^(harness_session_id|claude_session_id):' "$LIVE_STATE_FILE" 2>/dev/null \
        | sed -E 's/^(harness_session_id|claude_session_id):[[:space:]]*//' \
        | grep -Ev '^(null)?$' | head -1 | tr -d '[:space:]' || true)
    DELIVERY_RETRY_OWNER="${HOOK_HARNESS_ID:-${LIVE_HARNESS_ID:-harness}}"
    DELIVERY_RETRY_ID="${DELIVERY_RETRY_OWNER}.${LIVE_SESSION_ID:-session}"
    DELIVERY_PENDING_STATE="${DELIVERY_PENDING_PREFIX}${DELIVERY_RETRY_ID}.md"
    [[ -f "$DELIVERY_PENDING_STATE" ]] && STATE_FILE="$DELIVERY_PENDING_STATE"
else
    DELIVERY_PENDING_STATE=""
    for pending in "${DELIVERY_PENDING_PREFIX}${HOOK_HARNESS_ID}."*.md; do
        if [[ -n "$HOOK_HARNESS_ID" && -f "$pending" ]] \
            && { [[ -z "$DELIVERY_PENDING_STATE" ]] || [[ "$pending" -nt "$DELIVERY_PENDING_STATE" ]]; }; then
            DELIVERY_PENDING_STATE="$pending"
        fi
    done
    for pending in "${DELIVERY_PENDING_PREFIX}"*.md; do
        [[ -n "$DELIVERY_PENDING_STATE" ]] && break
        PENDING_HARNESS_ID=$(grep -E '^(harness_session_id|claude_session_id):' "$pending" 2>/dev/null \
            | sed -E 's/^(harness_session_id|claude_session_id):[[:space:]]*//' \
            | grep -Ev '^(null)?$' | head -1 | tr -d '[:space:]' || true)
        if [[ -n "$HOOK_HARNESS_ID" && "$PENDING_HARNESS_ID" == "$HOOK_HARNESS_ID" ]]; then
            DELIVERY_PENDING_STATE="$pending"
        fi
    done
    [[ -n "$DELIVERY_PENDING_STATE" ]] && STATE_FILE="$DELIVERY_PENDING_STATE"
fi
DELIVERY_CANDIDATE="${DELIVERY_PENDING_STATE}.candidate.$$"
trap 'rm -f "$DELIVERY_CANDIDATE" 2>/dev/null || true' EXIT

# Second candidate: a king session. Its manifest is a separate file because a
# king runs in the canonical checkout where a target manifest may also exist,
# and the target manifest wins when both are present: a session holding one is
# a worker, whatever else is on disk beside it. Checked only after the target
# search comes up empty, so nothing about a worker's path changes.
KING_STATE_FILE=""
DRIVER="target"
if [[ ! -f "$STATE_FILE" ]]; then
    if [[ "$TARGET_RESOLVE_BROKEN" -eq 1 ]]; then
        RCOUNT_FILE="${SPACE_DIR}/.loop-check-unavail-${RESOLVE_HARNESS_ID:-anon}"
        RCOUNT=0
        mkdir -p "$SPACE_DIR" 2>/dev/null || true
        [[ -f "$RCOUNT_FILE" ]] && RCOUNT=$(tr -dc '0-9' < "$RCOUNT_FILE" 2>/dev/null)
        [[ -n "$RCOUNT" ]] || RCOUNT=0
        RCOUNT=$((10#$RCOUNT + 1))
        echo "$RCOUNT" > "$RCOUNT_FILE" 2>/dev/null || true
        if (( RCOUNT <= MAX_UNAVAIL_RETRIES )); then
            emit_block_for_harness "checker unavailable (${RCOUNT}/${MAX_UNAVAIL_RETRIES}), keeping session running"
        fi
        echo "target stop-hook: manifest resolver unavailable ${RCOUNT} times; allowing visitor stop" >&2
        exit 0
    fi
    # Presence is NOT ownership. Kings run in the canonical checkout, which is
    # where every ordinary session also runs, and nothing deletes this manifest
    # when a king dies. Gating on the file alone therefore held every later
    # claude session in the repo open until the board was clean, for people who
    # never crowned anything, permanently.
    #
    # So the manifest must NAME this session. An id that is missing on either
    # side proves nothing, and the safe reading of "cannot prove it" is to let
    # the session go: a stale manifest outliving its king must not capture a
    # stranger. `fno agents king init` refuses to write an unattributable manifest, so
    # a real king always has an id to match.
    HOOK_HARNESS="${FNO_HARNESS:-}"
    if [[ -z "$HOOK_HARNESS" ]] && [[ "$HOOK_TRANSCRIPT_PATH" == "$HOME/.claude/projects/"* ]]; then
        # The transcript's location is proof; env markers are claims. A claude
        # session spawned from a codex parent inherits CODEX_THREAD_ID and
        # carries no claude marker, so the lone foreign marker below wins, the
        # crowned-row lookup narrows on the wrong harness, and the gate is
        # bypassed. Only ~/.claude/projects claims claude here: every other
        # transcript location keeps the marker logic unchanged.
        HOOK_HARNESS="claude"
    fi
    if [[ -z "$HOOK_HARNESS" ]]; then
        MARKER_COUNT=0
        [[ -n "${CODEX_THREAD_ID:-}" ]] && { HOOK_HARNESS="codex"; MARKER_COUNT=$((MARKER_COUNT + 1)); }
        [[ -n "${CLAUDE_CODE_SESSION_ID:-}" ]] && { HOOK_HARNESS="claude"; MARKER_COUNT=$((MARKER_COUNT + 1)); }
        [[ -n "${GEMINI_SESSION_ID:-}" ]] && { HOOK_HARNESS="gemini"; MARKER_COUNT=$((MARKER_COUNT + 1)); }
        [[ $MARKER_COUNT -gt 1 ]] && HOOK_HARNESS=""
    fi
    # Cheap pre-check: no scope manifests on disk means nothing can name this
    # session, and skipping the fno call keeps an ordinary session's stop at
    # one directory glob instead of a full Python CLI startup.
    KINGS_DIR="${SPACE_DIR}/kings"
    KING_RESOLVE_BROKEN=0
    if [[ -n "$HOOK_HARNESS_ID" ]] && compgen -G "${KINGS_DIR}/*.md" >/dev/null 2>&1; then
        if command -v fno >/dev/null 2>&1; then
            # The bare id, for the same reason the target resolver needs it:
            # `_find_by_session` does exact equality against the stamped
            # `harness_session_id`, so handing it a codex rollout basename
            # misses every time and disarms the king gate on codex silently.
            MANIFEST_ARGS=(agents king manifest-path --harness-session-id "${RESOLVE_HARNESS_ID:-$HOOK_HARNESS_ID}" --state-root "$SPACE_DIR")
            [[ -n "$HOOK_HARNESS" ]] && MANIFEST_ARGS+=(--harness "$HOOK_HARNESS")
            KING_RC=0
            KING_STATE_FILE=$(fno "${MANIFEST_ARGS[@]}" 2>/dev/null) || KING_RC=$?
            # Exit 1 with no path is the verb's "no manifest names this
            # session": a stranger goes free. Any OTHER nonzero exit is a
            # resolver that cannot answer (a stale deployed fno without the
            # verb exits 2), and allowing there would silently disarm a live
            # king where the old file grep never needed fno at all.
            if [[ "$KING_RC" -ne 0 && "$KING_RC" -ne 1 ]]; then
                KING_RESOLVE_BROKEN=1
            fi
        else
            KING_RESOLVE_BROKEN=1
        fi
    fi
    if [[ -n "$KING_STATE_FILE" && -f "$KING_STATE_FILE" ]]; then
        STATE_FILE="$KING_STATE_FILE"
        DRIVER="king"
    elif [[ "$KING_RESOLVE_BROKEN" == "1" ]]; then
        # Bounded block, same contract as unavailable_block_or_allow, keyed by
        # this transcript's id because SESSION_ID is not derived until a state
        # file is chosen. Loud give-up, never a silent allow.
        KCOUNT="${SPACE_DIR}/.king-resolve-unavail-${HOOK_HARNESS_ID:-anon}"
        KNUM=0
        [[ -f "$KCOUNT" ]] && KNUM=$(tr -dc '0-9' < "$KCOUNT" 2>/dev/null)
        [[ -z "$KNUM" ]] && KNUM=0
        KNUM=$((10#$KNUM + 1))
        echo "$KNUM" > "$KCOUNT" 2>/dev/null || true
        if (( KNUM <= MAX_UNAVAIL_RETRIES )); then
            emit_block_for_harness "king manifest resolver unavailable (${KNUM}/${MAX_UNAVAIL_RETRIES}) for an active kings dir, keeping session running"
        fi
        echo "target stop-hook: king manifest resolver unavailable ${KNUM} times (counter ${KCOUNT}); allowing stop (king gate off for this stop)" >&2
        exit 0
    else
        if [[ "$TARGET_NO_MATCH" -eq 1 ]]; then
            # Name every id that was tried, not just the first. This is the one
            # line a human reads when a session was let go, and this hook let
            # codex targets go silently for six days; a diagnostic that names
            # one of four attempts sends the next reader after the wrong id.
            # The marker itself is a contract (docs/architecture/unified-loop.md,
            # and hooks/agy-target-stop-hook.sh emits the same string), so the
            # detail is APPENDED and the matched prefix stays byte-identical.
            echo "loop-check: no manifest names session ${RESOLVE_HARNESS_ID}; visitor allowed (tried: ${RESOLVE_IDS[*]})" >&2
        fi
        exit 0
    fi
fi

# Active session confirmed from here down. A king manifest carries fno_id and
# no session_id, so read both; SESSION_ID only keys the unavailable-retry
# counter, and an empty one would collide every king with every other.
SESSION_ID=$(sed -n 's/^fno_id:[[:space:]]*//p' "$STATE_FILE" 2>/dev/null \
    | head -1 | tr -d '[:space:]' || true)
if [[ -z "$SESSION_ID" ]]; then
    SESSION_ID=$(sed -n 's/^session_id:[[:space:]]*//p' "$STATE_FILE" 2>/dev/null \
        | head -1 | tr -d '[:space:]' || true)
fi

PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-${CODEX_PLUGIN_ROOT:-${GEMINI_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}}}"
EVENTS_LIB="${PLUGIN_ROOT}/scripts/lib/events.sh"
# shellcheck source=../scripts/lib/events.sh
[[ -r "$EVENTS_LIB" ]] && source "$EVENTS_LIB" 2>/dev/null || true

# Append an event to both project + global logs WITHOUT jq (this runs on the
# jq-missing give-up path too). Fields are hook-internal and safe to interpolate.
emit_event_both() {
    local etype="$1" data="$2" ts line global_events
    ts=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)
    line="{\"ts\":\"${ts}\",\"type\":\"${etype}\",\"source\":\"hook\",\"data\":${data}}"
    if ! declare -F _append_bounded_event >/dev/null 2>&1; then
        echo "target stop-hook: event helper unavailable; skipping ${etype}" >&2
        return
    fi
    # Honors an overridden config.state_dir when a caller has sourced the shell
    # stub; falls back to the default so the jq-missing give-up path still logs.
    global_events="${GLOBAL_EVENTS_PATH:-${STATE_DIR:-$HOME/.fno}/events.jsonl}"
    _append_bounded_event target_stop_hook "$line" "${SPACE_DIR}/events.jsonl" || true
    _append_bounded_event target_stop_hook "$line" "$global_events" || true
}

# One control-plane arm row for this fire (x-1b88). interval_s 0: the stop hook
# is event-driven, so the arms readout never reads it stale from quiet.
emit_tick_row() {
    local acted="$1" skip="$2" detail="$3" skip_json
    if [[ -n "$skip" ]]; then skip_json="\"$skip\""; else skip_json="null"; fi
    emit_event_both "control_plane_tick" \
        "{\"arm\":\"stop_hook\",\"scheduler\":\"hook:target-stop-hook\",\"acted\":${acted},\"skip_reason\":${skip_json},\"detail\":\"$detail\",\"interval_s\":0}"
}

# Checker unavailable for an ACTIVE session: bounded-block, then loud give-up.
# Counter keyed by session_id so two sessions sharing a symlinked .fno never
# consume each other's retry budget (AC2-EDGE). Calls exit directly.
unavailable_block_or_allow() {
    emit_tick_row 0 checker_unavailable "driver=${DRIVER:-unknown} session=${SESSION_ID:-unknown}"
    local counter="${SPACE_DIR}/.loop-check-unavail-${SESSION_ID}"
    local count=0
    [[ -f "$counter" ]] && count=$(tr -dc '0-9' < "$counter" 2>/dev/null)
    [[ -z "$count" ]] && count=0          # absent or corrupt -> start at 0
    count=$((10#$count + 1))              # 10# so a stray leading zero isn't read as octal
    echo "$count" > "$counter" 2>/dev/null || true
    if (( count <= MAX_UNAVAIL_RETRIES )); then
        emit_block_for_harness "checker unavailable (${count}/${MAX_UNAVAIL_RETRIES}), keeping session running"
    fi
    emit_event_both "loop_check_unavailable_giveup" "{\"session_id\":\"${SESSION_ID}\",\"count\":${count}}"
    echo "target stop-hook: checker unavailable ${count} times; allowing stop (ship gate off for this stop)" >&2
    exit 0
}

if [[ "$TARGET_RESOLVE_BROKEN" -eq 1 ]]; then
    echo "target stop-hook: WARNING: manifest-for-session resolver unavailable" >&2
    unavailable_block_or_allow
fi

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

# A non-claude harness writes this manifest too, and leaves claude_session_id
# empty while recording its own id in harness_session_id. Read that second, or
# the guard is inert for every manifest claude did not author and a claude
# session stopping in a codex session's worktree is judged against a run it does
# not own. Both harnesses wire THIS hook (hooks/hooks.json and
# hooks/codex-hooks.json), so the guard has to answer for both.
if [[ -z "$MANIFEST_CTID" || "$MANIFEST_CTID" == "null" ]]; then
    MANIFEST_CTID=$(grep -E '^harness_session_id:' "$STATE_FILE" 2>/dev/null \
        | head -1 | sed -E 's/^harness_session_id:[[:space:]]*//' | tr -d '[:space:]')
fi

# "null" = init ran without transcript-id env vars (diagnostic/non-Claude
# starts); treat it like empty so the guard never disables the hook (codex
# P2 on #447).
if [[ -n "$MANIFEST_CTID" && "$MANIFEST_CTID" != "null" ]]; then
    TRANSCRIPT_BASENAME=$(basename "$TRANSCRIPT_PATH" .jsonl)
    # Claude names a transcript for its session id, so the basename IS the id.
    # Codex names one rollout-<utc>-<thread-uuid>, so the id is a SUFFIX. A
    # plain equality test therefore reads a codex owner's own stop as foreign
    # and exits 0, which does not wedge it - it silently turns the ship gate off
    # for every codex target run. Accept either shape.
    if [[ "$TRANSCRIPT_BASENAME" != "$MANIFEST_CTID" \
        && "$TRANSCRIPT_BASENAME" != *"-$MANIFEST_CTID" ]]; then
        # Another session's manifest; genuinely not ours to judge.
        exit 0
    fi
fi

# ── 5. Resolve the binary ─────────────────────────────────────────────────────
[[ -n "$BIN" ]] || BIN=$(resolve_agents_bin)

# ── 6. Binary missing for an active session: emit event + bounded-block ───────
# A stale/absent binary must not silently disable the ship gate; emit the
# diagnostic (as before) then route through the bounded-block helper.
if [[ -z "$BIN" ]]; then
    emit_event_both "loop_check_binary_missing" "{\"session_id\":\"${SESSION_ID}\"}"
    emit_tick_row 0 binary_missing "driver=${DRIVER:-unknown} session=${SESSION_ID:-unknown}"
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
mkdir -p "$SPACE_DIR" 2>/dev/null || true
CANDIDATE_READY=0
if [[ "$STATE_FILE" != "$DELIVERY_PENDING_STATE" ]] \
    && cp "$STATE_FILE" "$DELIVERY_CANDIDATE" 2>/dev/null; then
    CANDIDATE_READY=1
fi
LOOP_CHECK_LOG="${SPACE_DIR}/loop-check.stderr.log"
DECISION_JSON=""
verb_rc=0
if [[ "$STATE_FILE" == "$DELIVERY_PENDING_STATE" ]]; then
    DECISION_JSON='{"decision":"allow","termination_reason":"DoneDelivery","message":"retrying generic delivery finalization"}'
else
    # One settle sweep before the checker, TARGET loops only: a lost review
    # invocation becomes a `lost` attestation row, so the gate's next read
    # answers a named refusal instead of waiting on silence. A king driver
    # owns no review invocations, and its manifest resolution must stay the
    # hook's last `fno` call. Best-effort and quiet - a missing fno or a
    # failed sweep must never hold the stop gate, and the doctor's report
    # re-runs the sweep on its own schedule.
    if [[ "$DRIVER" == "target" ]] && command -v fno >/dev/null 2>&1; then
        (cd "$TARGET_CWD" 2>/dev/null \
            && fno do review invocations settle >/dev/null 2>&1) || true
    fi
    DECISION_JSON=$("$BIN" loop-check \
        --driver "$DRIVER" \
        --state "$STATE_FILE" \
        --transcript "$TRANSCRIPT_PATH" \
        --cwd "$TARGET_CWD" \
        --hook-input-stdin \
        2>>"$LOOP_CHECK_LOG" <<<"$HOOK_INPUT") || verb_rc=$?

    # loop-check reports CLI misuse as {"error": ...} on STDOUT and exits 2,
    # writing NOTHING to stderr (loopcheck.rs decide_inner: "Missing required
    # flags are CLI misuse: exit 2"). Tailing the stderr log alone therefore
    # prints an EMPTY file for that whole error class, and an operator watches
    # a ship gate degrade with no stated cause. We already captured the answer
    # in DECISION_JSON; print it before falling back to the log.
    report_loop_check_output() {
        if [[ -n "$DECISION_JSON" ]]; then
            echo "target stop-hook: loop-check output: ${DECISION_JSON}" >&2
        else
            echo "target stop-hook: loop-check produced no output at all" >&2
        fi
        tail -n 5 "$LOOP_CHECK_LOG" >&2 2>/dev/null || true
    }
    # A non-zero exit means a BROKEN checker only when the output carries no
    # verdict. The king driver sends a full decision payload on both block
    # shapes (loopcheck.rs king_decide): exit 0 for a healthy non-empty board,
    # exit 2 when the board is unreadable, while CLI misuse returns 2 with
    # {"error": ...} and no `decision` field. So key on the field, never the
    # code: honor any verdict, and call unavailable only a verdictless reply.
    # Reading the code alone counted every legitimate king block as an outage,
    # never reset the retry counter (it resets only on a clean decision), and
    # at MAX_UNAVAIL_RETRIES released the very king it meant to hold - under a
    # loop_check_unavailable_giveup event blaming a checker that had answered
    # correctly every time.
    if ! echo "$DECISION_JSON" | jq -e '.decision' >/dev/null 2>&1; then
        if [[ $verb_rc -ne 0 ]]; then
            echo "target stop-hook: WARNING: fno-agents loop-check exited $verb_rc with no decision for an active session" >&2
        else
            echo "target stop-hook: WARNING: fno-agents loop-check returned unexpected output (not JSON) for an active session" >&2
        fi
        report_loop_check_output
        unavailable_block_or_allow
    fi
fi

# ── 8. Clean decision reached: self-heal the unavailable counter ──────────────
# The checker worked. Reset the consecutive-failure counter FIRST so the bound
# is on consecutive failures only and a recovered checker starts fresh (AC2-FR).
rm -f "${SPACE_DIR}/.loop-check-unavail-${SESSION_ID}" 2>/dev/null || true

# ── 9. Translate decision to hook protocol ────────────────────────────────────
DECISION=$(echo "$DECISION_JSON" | jq -r '.decision // "allow"')
MESSAGE=$(echo "$DECISION_JSON" | jq -r '.message // ""')
TERMINATION_REASON=$(echo "$DECISION_JSON" | jq -r '.termination_reason // empty')
emit_tick_row 1 "" "driver=${DRIVER:-unknown} decision=${DECISION} reason=${TERMINATION_REASON:-live}"

if [[ "$DECISION" == "block" ]]; then
    emit_block_for_harness "$MESSAGE"
fi

# ── 10. Terminal-allow: invoke the finalize WRITER (step 6, ab-f8e5f214) ───────
# On a terminal allow, the shim runs the separate `finalize` writer for the
# ledger record and ship-only stamp, graduation, and handoff in every mode.
#
# Legacy finalize remains idempotent and best-effort. Generic delivery is the
# exception: its receipt, stamp, and handoff are part of the terminal contract,
# so a non-zero writer exit keeps the session alive for an idempotent retry.
# Run synchronously (NOT backgrounded): a backgrounded child would be SIGHUP'd
# when the session process exits, defeating the survive-compaction goal.
# A king terminal skips this whole block. `finalize` stamps a plan, graduates a
# node, and writes a ledger row for ONE deliverable; a king has none of those,
# and pointing it at a king manifest would have it read fields that are not
# there. The claim release below is likewise target-shaped: a king manifest
# carries no target_claim_key, so it would be a no-op even unguarded.
if [[ -n "$TERMINATION_REASON" && "$DRIVER" == "king" ]]; then
    echo "target stop-hook: king terminal ($TERMINATION_REASON); the king loop has no plan to stamp" >&2
elif [[ -n "$TERMINATION_REASON" ]]; then
    # ── 10b. Release the node claim at a FINISHED terminal ─────────────
    # Fires BEFORE finalize on purpose: both stamp a `do` row for the same
    # session, and sessions[] is append-only (first observation wins), so the
    # release row must land first to carry its ended_at window; finalize's later
    # stamp collapses. A terminal that means the node's work is FINISHED frees
    # its claim now (wiring the intent section 11 states and no code executed -
    # a real doc/code divergence). A terminal that only means THIS SESSION
    # STOPPED (PR open, more work coming) KEEPS holding, so a stopped-but-
    # resumable session is not handed to a twin. CHANGES CLAIM LIFECYCLE:
    # finished-terminal releases instead of stale-after-TTL.
    #
    # SOURCE OF TRUTH: the variants below are a hand-maintained mirror of the
    # Rust `TerminationReason` enum in crates/fno-agents/src/loopcheck.rs. The
    # four deliberately omitted clean terminals (DoneBatched, DoneAwaitingMerge,
    # DoneUnreviewed, DonePlanned) keep their claim because no further agent
    # work is coming but a human/batch merge or the reconcile path still owes
    # the close - releasing here would hand a twin dispatcher the node mid-flight.
    # When Rust gains a new Done* variant, update both this list and the
    # classification in loopcheck.rs; the deeper fix is a `releases_claim: bool`
    # field on the decision JSON so this stop pattern-matching goes away.
    case "$TERMINATION_REASON" in
        DonePRGreen|DoneAdvisory|DoneDelivery|NoWork)
            _REL_KEY="$(sed -n 's/^target_claim_key:[[:space:]]*"\([^"]*\)".*/\1/p' "$STATE_FILE" 2>/dev/null | head -1)"
            _REL_HOLDER="$(sed -n 's/^target_claim_holder:[[:space:]]*"\([^"]*\)".*/\1/p' "$STATE_FILE" 2>/dev/null | head -1)"
            if [[ "$_REL_KEY" == node:* && -n "$_REL_HOLDER" ]]; then
                FNO_CLAIMS_ROOT="$HOME" fno agents claim release "$_REL_KEY" \
                    --holder "$_REL_HOLDER" --stamp-do \
                    >/dev/null 2>>"${SPACE_DIR}/loop-check.stderr.log" || true
            fi
            ;;
    esac
    FINALIZE_STATE="$STATE_FILE"
    if [[ "$TERMINATION_REASON" == "DoneDelivery" ]]; then
        if [[ "$STATE_FILE" != "$DELIVERY_PENDING_STATE" ]] \
            && { [[ $CANDIDATE_READY -ne 1 ]] \
                || ! mv "$DELIVERY_CANDIDATE" "$DELIVERY_PENDING_STATE"; }; then
            emit_block_for_harness "generic delivery state could not be preserved; will retry"
        fi
        FINALIZE_STATE="$DELIVERY_PENDING_STATE"
    fi
    # Log the full trace and surface failures to the attended operator.
    FINALIZE_OUT=""
    FINALIZE_RC=0
    FINALIZE_OUT="$("$BIN" finalize \
        --state "$FINALIZE_STATE" \
        --transcript "$TRANSCRIPT_PATH" \
        --cwd "$TARGET_CWD" \
        --reason "$TERMINATION_REASON" 2>&1)" || FINALIZE_RC=$?
    if [[ -n "$FINALIZE_OUT" ]]; then
        printf '%s\n' "$FINALIZE_OUT" >> "${SPACE_DIR}/finalize.stderr.log" 2>/dev/null || true
    fi
    if [[ $FINALIZE_RC -ne 0 ]] || printf '%s' "$FINALIZE_OUT" | grep -qi 'failed'; then
        echo "target stop-hook: finalize note (non-blocking): $(printf '%s' "$FINALIZE_OUT" | tail -n 3)" >&2
    fi
    if [[ "$TERMINATION_REASON" == "DoneDelivery" && $FINALIZE_RC -ne 0 ]]; then
        emit_block_for_harness "generic delivery finalization failed; will retry"
    fi
    if [[ "$TERMINATION_REASON" == "DoneDelivery" ]]; then
        if ! rm -f "$DELIVERY_PENDING_STATE" 2>/dev/null; then
            emit_block_for_harness "generic delivery retry state cleanup failed; will retry"
        fi
    fi
fi

# ── 11. Live-tick: refresh this session's node claim so a long-running loop ──
# never silently expires its TTL and frees the node for a twin (x-a7ab 1.4).
# Best-effort, non-blocking: any failure (no claim, holder mismatch after a
# supervisor respawn, stale manifest snapshot, fno absent) is logged and ignored
# - it can never change the completion decision. Skipped on a TERMINAL allow: a
# session that is done must not extend a claim it is about to release. --ttl is
# always passed (default 2h) so refresh keeps the original window rather than
# shrinking it to MIN_TTL_MS.
if [[ -z "$TERMINATION_REASON" && -f "$STATE_FILE" ]]; then
    _TC_KEY="$(sed -n 's/^target_claim_key:[[:space:]]*"\([^"]*\)".*/\1/p' "$STATE_FILE" 2>/dev/null | head -1)"
    _TC_HOLDER="$(sed -n 's/^target_claim_holder:[[:space:]]*"\([^"]*\)".*/\1/p' "$STATE_FILE" 2>/dev/null | head -1)"
    _TC_TTL="$(sed -n 's/^target_claim_ttl:[[:space:]]*"\([^"]*\)".*/\1/p' "$STATE_FILE" 2>/dev/null | head -1)"
    if [[ "$_TC_KEY" == node:* && -n "$_TC_HOLDER" ]]; then
        FNO_CLAIMS_ROOT="$HOME" fno agents claim refresh "$_TC_KEY" \
            --holder "$_TC_HOLDER" --ttl "${_TC_TTL:-2h}" \
            >/dev/null 2>>"${SPACE_DIR}/loop-check.stderr.log" || true
    fi
fi

# allow (includes DonePRGreen, DoneAdvisory, DoneDelivery, NoWork, Budget, NoProgress, etc.)
echo "target stop-hook: $MESSAGE" >&2
exit 0
