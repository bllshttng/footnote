#!/usr/bin/env bash
# hooks/king-context-nudge.sh - Stop hook: one crown read, two checks, one boundary.
#
# A crowned king ending a turn gets TWO checks at the same boundary, sharing one
# registry read:
#   (a) CONTEXT - is this king past king_used_pct_trigger? A king hands off
#       earlier than a teammate (40 vs 50) because its degradation propagates
#       into every ruling it issues and every worker it routes, and the handoff
#       itself costs context.
#   (b) ORPHANS - did this king spawn workers that are still live with no
#       resolution recorded? A reign that spawns workers cannot be a pure pass:
#       abdicating now leaves them nobody to mail when they reach review.
#
# Both BLOCK (the only Stop output documented to reach the model on Claude and
# Codex; an allow's systemMessage is informational-only / lost on Codex) and
# emit an event (king_context_nudge / king_orphan_block) so `fno event audit` can
# prove the hook actually fired in a real reign. That arming proof is the one
# question its predecessor (arm-handoff-precompact.sh, gated on a pid dead ~1s
# after init) could never answer.
#
# EVERY error path exits 0 with no block output. A non-zero Stop hook is not an
# allow, and a registry or probe problem must never start blocking every crowned
# session's turn end. The check degrades toward silence, never toward a false
# handoff. The same fail-safe direction as the probe: unreadable -> no pressure.
#
# What it must NOT gate on (the arm-handoff lesson, AC9): no pid-liveness probe
# (a pid can only prove life, never death, and the init wrapper pid died ~1s
# after init), no read of the target manifest (a king pass has none, so keying
# on one would deliver this to zero kings), no reconstructed session identity.
# Every signal here is either handed to the hook in its payload (transcript_path,
# session_id) or read from live external state (the registry, the carveout
# ledger, config) at fire time.
set -uo pipefail

PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/with-timeout.sh
source "$PLUGIN_ROOT/scripts/lib/with-timeout.sh" 2>/dev/null || exit 0

FNO_DIR="${FNO_DIR:-.fno}"
REPO_ROOT=$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || echo "$PWD")

emit_event() {
    # Append to both project + global logs (no jq; safe interpolation). Same
    # shape target-stop-hook's emit_event_both uses.
    local etype="$1" data="$2" ts line
    ts=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null)
    line="{\"ts\":\"${ts}\",\"type\":\"${etype}\",\"source\":\"hook\",\"data\":${data}}"
    mkdir -p "${REPO_ROOT}/.fno" "${HOME}/.fno" 2>/dev/null || true
    echo "$line" >> "${REPO_ROOT}/.fno/events.jsonl" 2>/dev/null || true
    echo "$line" >> "${HOME}/.fno/events.jsonl" 2>/dev/null || true
}

# ── 1. Read stdin: transcript_path + session_id from the Stop payload ─────────
# These are the two identifiers the hook is HANDED. It reconstructs nothing.
HOOK_INPUT=$(cat)
TRANSCRIPT=$(printf '%s' "$HOOK_INPUT" | sed -n \
    's/.*"transcript_path"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
KING_SESSION_ID=$(printf '%s' "$HOOK_INPUT" | sed -n \
    's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
[[ -z "$KING_SESSION_ID" ]] && KING_SESSION_ID="${CLAUDE_CODE_SESSION_ID:-}"

# No transcript handed to us -> not a real crowned-session fire. The context
# check needs it; an orphan-only fire on a payload with no transcript_path would
# fire on synthetic input. Exit 0.
[[ -z "$TRANSCRIPT" || ! -f "$TRANSCRIPT" ]] && exit 0
# transcript basename is the one identifier the hook is HANDED rather than
# reconstructs, so latches key on it: two sessions sharing a symlinked .fno
# never consume each other's latch (same fix target-stop-hook applied).
TBASE="$(basename "$TRANSCRIPT" .jsonl 2>/dev/null || echo "$TRANSCRIPT")"

# ── 2. King trigger from config (default 40). ─────────────────────────────────
KING_TRIGGER="40"
if command -v fno >/dev/null 2>&1; then
    _t=$(with_timeout 3 fno config get target.handoff.king_used_pct_trigger 2>/dev/null || true)
    case "$_t" in
        ''|*[!0-9]*) ;;          # unreadable / non-numeric -> keep default 40
        *) KING_TRIGGER="$_t" ;;
    esac
fi

# ── 3. Context reading (one probe). Unreadable -> no pressure. ────────────────
USED_PCT=""
USED_TOKENS=""
WINDOW_TOKENS=""
if command -v fno >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
    PROBE_OUT=$(with_timeout 5 fno context --transcript "$TRANSCRIPT" --json 2>/dev/null || true)
    # jq, not sed: BSD sed (macOS) does not support `[0-9]\+` in basic regex, and
    # the hook already requires jq for the registry read below.
    _p=$(printf '%s' "$PROBE_OUT" | jq -r '.used_pct // empty' 2>/dev/null)
    case "$_p" in
        ''|*[!0-9]*) ;;          # unreadable -> USED_PCT stays empty (no pressure)
        *) USED_PCT="$_p"
           USED_TOKENS=$(printf '%s' "$PROBE_OUT" | jq -r '.used_tokens // empty' 2>/dev/null)
           WINDOW_TOKENS=$(printf '%s' "$PROBE_OUT" | jq -r '.window_tokens // empty' 2>/dev/null) ;;
    esac
fi

# ── 4. One registry read: this session's crown + its live children. ───────────
# `fno agents registry-json` is a daemon-free file read (load_registry), NOT
# `fno agents list` (which is Rust-routed and lazy-starts the daemon for
# live-status enrichment - a Stop hook must never stall on a daemon start).
# Emits structured crown_level/crown_scope/spawned_by_session per row, so one
# bounded call gives both the crown gate and the orphan list.
AGENTS_JSON=""
if command -v fno >/dev/null 2>&1; then
    AGENTS_JSON=$(with_timeout 5 fno agents registry-json 2>/dev/null || true)
fi
if ! printf '%s' "$AGENTS_JSON" | jq -e '.agents' >/dev/null 2>&1; then
    # Registry unreadable -> cannot confirm a crown -> degrade silent (exit 0).
    # A registry problem must never block every crowned session's turn end.
    exit 0
fi

# This session's row (by session_id). No row or no crown_level -> not a king.
MY_ROW=$(printf '%s' "$AGENTS_JSON" | jq -c --arg sid "$KING_SESSION_ID" \
    '.agents[] | select(.session_id == $sid or .harness_session_id == $sid)' 2>/dev/null | head -1)
[[ -z "$MY_ROW" ]] && exit 0
CROWN_LEVEL=$(printf '%s' "$MY_ROW" | jq -r '.crown_level // empty' 2>/dev/null)
[[ -z "$CROWN_LEVEL" ]] && exit 0     # no crown -> neither check applies
CROWN_SCOPE=$(printf '%s' "$MY_ROW" | jq -r '.crown_scope // empty' 2>/dev/null)

# Active children this king spawned. Active = NOT in the terminal set
# (exited/orphaned/failed/permanent_dead); covers spawning, ready, idle, busy,
# live, restarting - all are workers that may still need their king at review.
ORPHANS=$(printf '%s' "$AGENTS_JSON" | jq -r --arg sid "$KING_SESSION_ID" \
    '[.agents[] | select(.spawned_by_session == $sid and ((.status // "exited") | IN("exited";"orphaned";"failed";"permanent_dead") | not))] | map(.name) | join(", ")' \
    2>/dev/null)
ORPHAN_COUNT=$(printf '%s' "$ORPHANS" | wc -w | tr -d ' ')

# ── 5. Band index (shared by both latches). No reading -> band "0" so the ─────
# orphan check can still latch once; the context check is skipped without a reading.
BAND="0"
if [[ -n "$USED_PCT" ]]; then
    BAND=$(( USED_PCT / 10 ))
fi
CTX_LATCH="${FNO_DIR}/.king-nudge-ctx-${TBASE}-${BAND}"
ORPHAN_LATCH="${FNO_DIR}/.king-nudge-orphan-${TBASE}-${BAND}"

REASON=""

# ── 6. Check (a): context pressure. ──────────────────────────────────────────
if [[ -n "$USED_PCT" && "$USED_PCT" -ge "$KING_TRIGGER" && ! -f "$CTX_LATCH" ]]; then
    touch "$CTX_LATCH" 2>/dev/null || true
    emit_event "king_context_nudge" \
        "{\"used_pct\":${USED_PCT},\"trigger\":${KING_TRIGGER},\"crown_level\":${CROWN_LEVEL},\"crown_scope\":\"${CROWN_SCOPE}\",\"session_id\":\"${KING_SESSION_ID}\"}"
    REASON="context: ${USED_PCT}% used (${USED_TOKENS:-?} of ${WINDOW_TOKENS:-?} tokens). You hold a crown (level ${CROWN_LEVEL}, scope ${CROWN_SCOPE}) and are past the king handoff trigger (${KING_TRIGGER}%). Hand off before you abdicate: a crowned session that abdicates at kickoff orphans every worker it spawned. Next: bash skills/target/scripts/handoff.sh, or spawn your successor and run 'fno agents crown <successor> --scope ${CROWN_SCOPE} --succeed', closing this pane only after the successor's session header prints."
fi

# ── 7. Check (b): orphaned live children (latches INDEPENDENTLY of context). ─
if [[ "$ORPHAN_COUNT" -gt 0 && ! -f "$ORPHAN_LATCH" ]]; then
    # Resolution 3: a carveout carrying THIS scope (structured field, not free
    # text) means the king stated the orphaning and fell back to advisory
    # self-review. Scope match is the discriminator, or any carveout silences it.
    RESOLVED=0
    if command -v fno >/dev/null 2>&1; then
        CARVEOUTS=$(with_timeout 3 fno carveout list --json 2>/dev/null || true)
        # carveout list --json emits JSONL (one object per line), so stream-filter
        # rather than map (which needs an array). Structured .scope field match,
        # Only a RECENT carveout (last 24h) counts: a historical one from a
        # previous reign must not permanently suppress orphan warnings for later
        # reigns over the same scope. ISO-8601 UTC strings compare lexicographically.
        _cutoff=$(date -u -v-24H '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
            || date -u -d '24 hours ago' '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || echo '')
        if [[ -n "$_cutoff" ]]; then
            _hit=$(printf '%s' "$CARVEOUTS" | jq --arg s "$CROWN_SCOPE" --arg c "$_cutoff" \
                'select(.scope == $s and (.ts // "0") >= $c)' 2>/dev/null | grep -c . || true)
        else
            _hit=$(printf '%s' "$CARVEOUTS" | jq --arg s "$CROWN_SCOPE" \
                'select(.scope == $s)' 2>/dev/null | grep -c . || true)
        fi
        [[ "$_hit" =~ ^[0-9]+$ && "$_hit" -gt 0 ]] && RESOLVED=1
    fi
    if [[ "$RESOLVED" -eq 0 ]]; then
        touch "$ORPHAN_LATCH" 2>/dev/null || true
        emit_event "king_orphan_block" \
            "{\"crown_level\":${CROWN_LEVEL},\"crown_scope\":\"${CROWN_SCOPE}\",\"workers\":\"${ORPHANS}\",\"count\":${ORPHAN_COUNT},\"session_id\":\"${KING_SESSION_ID}\"}"
        ORPHAN_REASON="You hold a crown (level ${CROWN_LEVEL}, scope ${CROWN_SCOPE}) and ${ORPHAN_COUNT} worker(s) you spawned are still live (${ORPHANS}). A reign that spawns workers cannot be a pure pass: abdicating now leaves them with nobody to mail when they reach review. Pick one and act, then this stops: (1) stay as court through the wave; (2) hand the crown over with 'fno agents crown <handle> --scope ${CROWN_SCOPE} --succeed'; (3) record that these workers are review-orphaned with 'fno carveout add -k deferred --scope ${CROWN_SCOPE} \"...\"' and they fall back to advisory self-review."
        if [[ -n "$REASON" ]]; then
            REASON="${REASON}  ||  ${ORPHAN_REASON}"
        else
            REASON="$ORPHAN_REASON"
        fi
    fi
fi

# ── 8. Emit one block decision if either check fired; else allow (exit 0). ────
if [[ -n "$REASON" ]]; then
    # block is the only Stop output that reaches the model on all harnesses.
    jq -nc --arg r "$REASON" '{decision:"block", reason:$r}' 2>/dev/null \
        || printf '{"decision":"block","reason":%s}\n' "$(jq -naR --arg r "$REASON" '$r' 2>/dev/null || printf '"%s"' "$REASON")"
    exit 0
fi
exit 0
