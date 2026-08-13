#!/usr/bin/env bash
# hooks/context-nudge.sh - Stop hook: one context probe for EVERY session, one
# crown-only orphan check. Used to be king-only (king-context-nudge.sh); the
# context nudge generalizes to every session because the probe already ran for
# every session and was discarded at the old crown gate.
#
# EVERY session gets the CONTEXT check (a): past the context trigger, block once
# per 10% band. The trigger is crown-shaped: a crowned king is nudged EARLIER
# (king_used_pct_trigger, default 40) than any other session (used_pct_trigger,
# default 50), because a king's degradation propagates into every ruling it issues
# and every worker it routes.
#
# What the trigger asks for is a COMPACT, for a king as much as anyone. A crown is
# maintained across a compact, so a king at high context compacts and keeps
# ruling; the branch used to read this percentage as a handoff trigger and pointed
# kings at the more expensive move. Handoff is a judgement about ruling QUALITY -
# worse calls, lost threads, repetition - and no percentage measures that, so the
# crowned branch asks the king to assess its own rulings and attaches no number to
# the answer. The concrete cost that makes compact the default: a successor gets a
# NEW mail handle, orphaning every worker still holding the old one.
#
# A CROWNED king additionally gets the ORPHAN check (b): did it spawn workers
# that are still live with no resolution recorded? A reign that spawns workers
# cannot be a pure pass: abdicating now leaves them nobody to mail at review.
# Check (b) is crown-only; only check (a) generalizes.
#
# Both BLOCK (the only Stop output documented to reach the model on Claude and
# Codex; an allow's systemMessage is informational-only / lost on Codex) and
# emit an event (session_context_nudge / king_context_nudge / king_orphan_block)
# so `fno event audit` can prove the hook actually fired in a real session. That
# arming proof is the one question its predecessor (arm-handoff-precompact.sh,
# gated on a pid dead ~1s after init) could never answer.
#
# The context check does NOT depend on the registry. The probe (`fno context`)
# is the only truthful pressure source: it counts tokens from the transcript and
# owns its denominator. The registry is BEST-EFFORT here, used only to pick the
# king trigger + king message and to run the orphan check. A missing row or an
# unreadable registry is treated as non-crowned, which still nudges only on REAL
# transcript pressure, so a registry problem can never produce a false handoff.
# This is why context pressure has ONE measurement path; a second-hand percentage
# whose denominator we cannot see was deleted from spend-drift-monitor.js for cause.
#
# EVERY error path exits 0 with no block output. A non-zero Stop hook is not an
# allow, and a probe/registry problem must never start blocking every session's
# turn end. The check degrades toward silence, never toward a false handoff. The
# same fail-safe direction as the probe: unreadable -> no pressure.
#
# What it must NOT gate on (the arm-handoff lesson, AC9): no pid-liveness probe
# (a pid can only prove life, never death, and the init wrapper pid died ~1s
# after init), no read of the target manifest (a king pass has none, so keying
# on one would deliver this to zero kings), no reconstructed session identity.
# Every GATING signal here is either handed to the hook in its payload
# (transcript_path, session_id) or read from live external state (the registry,
# the carveout ledger, config) at fire time. The one place that does touch ambient
# identity is the compact-path check in 5b, which asks `--to-self` because the
# verdict differs by recipient; it only WORDS an already-fired nudge, gates
# nothing, and a contaminated or absent identity lands on the unmeasurable branch.
set -uo pipefail

PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/with-timeout.sh
source "$PLUGIN_ROOT/scripts/lib/with-timeout.sh" 2>/dev/null || exit 0

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
SESSION_ID=$(printf '%s' "$HOOK_INPUT" | sed -n \
    's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
[[ -z "$SESSION_ID" ]] && SESSION_ID="${CLAUDE_CODE_SESSION_ID:-}"

# stop_hook_active:true means Stop re-fired after a previous block - the session
# is mid-compaction (our block told it to compact; the summarization turn is a
# model turn, so CC fires Stop on it) or otherwise continuing. Re-blocking loops;
# nudging it is noise. This is the one continuation signal the Stop payload
# carries (there is no compaction-specific field), so exit 0 the instant we see it.
STOP_HOOK_ACTIVE=$(printf '%s' "$HOOK_INPUT" | sed -n \
    's/.*"stop_hook_active"[[:space:]]*:[[:space:]]*\([a-z]*\).*/\1/p' | head -1)
[[ "$STOP_HOOK_ACTIVE" == "true" ]] && exit 0

# No transcript handed to us -> not a real fire. The context check needs it; an
# orphan-only fire on a payload with no transcript_path would fire on synthetic
# input. Exit 0.
[[ -z "$TRANSCRIPT" || ! -f "$TRANSCRIPT" ]] && exit 0
# transcript basename is the one identifier the hook is HANDED rather than
# reconstructs, so latches key on it: two sessions sharing a symlinked .fno
# never consume each other's latch (same fix target-stop-hook applied).
TBASE="$(basename "$TRANSCRIPT" .jsonl 2>/dev/null || echo "$TRANSCRIPT")"

# ── 2. Both triggers from config (general 50, king 40). ───────────────────────
GENERAL_TRIGGER="50"
KING_TRIGGER="40"
if command -v fno >/dev/null 2>&1; then
    _t=$(with_timeout 3 fno config get target.handoff.used_pct_trigger 2>/dev/null || true)
    case "$_t" in
        ''|*[!0-9]*) ;;          # unreadable / non-numeric -> keep default 50
        *) GENERAL_TRIGGER="$_t" ;;
    esac
    _t=$(with_timeout 3 fno config get target.handoff.king_used_pct_trigger 2>/dev/null || true)
    case "$_t" in
        ''|*[!0-9]*) ;;          # unreadable / non-numeric -> keep default 40
        *) KING_TRIGGER="$_t" ;;
    esac
fi

# ── 2b. Window gate + capacity floor (hook constants, not config). ───────────
# Two reasons to nudge, because a percentage measures different things at
# different window sizes. On a 1M lane the trigger is a QUALITY threshold
# (returns diminish long before the cap); on a 200k lane the preamble alone is a
# large fraction of the cap, so a percentage measures the preamble, not the work.
# The gate is on window_tokens (a number the probe returns), not a harness name,
# so a new lane classifies itself with no table to extend.
MIN_WINDOW="400000"   # below this only the capacity branch can fire
# RESERVE is a rung on Claude Code's own ladder, not an invented number: CC's
# real auto-compact trigger is effectiveWindow - 13000 (absolute), warn 20000
# below it. 80k remaining -> this nudge; 33k -> CC warn; 13k -> CC auto-compact.
RESERVE="80000"

# Flush-cadence thresholds. The plan's directive is to measure a real session
# distribution rather than invent these - a nag that fires mid-refactor gets
# ignored, and an ignored gate is worse than none. These are conservative
# starting points: only substantial stale accumulation (many files, static for a
# few turn-ends) trips it. Tune once a distribution is sampled.
FLUSH_FILE_THRESHOLD="10"   # uncommitted files above which stale work is at risk
FLUSH_STATIC_STOPS="3"      # consecutive turn-ends the HEAD must be unmoved

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

# ── 4. Registry read (BEST-EFFORT): crown + live children. ────────────────────
# `fno agents registry-json` is a daemon-free file read (load_registry), NOT
# `fno agents list` (which is Rust-routed and lazy-starts the daemon for
# live-status enrichment - a Stop hook must never stall on a daemon start).
# Emits structured crown_level/crown_scope/spawned_by_session per row.
#
# BEST-EFFORT: a missing/unreadable registry or a session with no row is treated
# as non-crowned. The context check still fires at the general trigger on REAL
# pressure; the orphan check (crown-only) is skipped. The registry refines WHICH
# trigger and message apply; it is never the source of pressure truth.
CROWN_LEVEL=""
CROWN_SCOPE=""
ORPHANS=""
ORPHAN_COUNT=0
if command -v fno >/dev/null 2>&1; then
    AGENTS_JSON=$(with_timeout 5 fno agents registry-json 2>/dev/null || true)
    if printf '%s' "$AGENTS_JSON" | jq -e '.agents' >/dev/null 2>&1; then
        # This session's row (by session_id). No row -> non-crowned (not an exit).
        MY_ROW=$(printf '%s' "$AGENTS_JSON" | jq -c --arg sid "$SESSION_ID" \
            '.agents[] | select(.session_id == $sid or .harness_session_id == $sid)' 2>/dev/null | head -1)
        if [[ -n "$MY_ROW" ]]; then
            CROWN_LEVEL=$(printf '%s' "$MY_ROW" | jq -r '.crown_level // empty' 2>/dev/null)
            CROWN_SCOPE=$(printf '%s' "$MY_ROW" | jq -r '.crown_scope // empty' 2>/dev/null)
        fi
        # Active children this session spawned. Computed ONLY when crowned: the
        # orphan check below is crown-only, so scanning the registry for children
        # on every non-king Stop (the common case) is wasted work on a hot path.
        # Active = NOT in the terminal set (exited/orphaned/failed/permanent_dead);
        # covers spawning, ready, idle, busy, live, restarting - all are workers
        # that may still need their king at review. Uses explicit inequalities
        # (not jq IN(), which is jq 1.7+ and absent on CI's ubuntu jq 1.6 - the
        # P1 fix's first attempt used IN() and failed silently on Linux).
        if [[ -n "$CROWN_LEVEL" ]]; then
            ORPHANS=$(printf '%s' "$AGENTS_JSON" | jq -r --arg sid "$SESSION_ID" '
                [.agents[] | select(
                    .spawned_by_session == $sid
                    and (.status // "exited") != "exited"
                    and (.status // "exited") != "orphaned"
                    and (.status // "exited") != "failed"
                    and (.status // "exited") != "permanent_dead"
                )] | map(.name) | join(", ")' \
                2>/dev/null)
            ORPHAN_COUNT=$(printf '%s' "$ORPHANS" | wc -w | tr -d ' ')
        fi
    fi
fi

# Crowned -> king trigger (40) + king posture; every other session -> general
# trigger (50). The crown determination is best-effort (above); a registry miss
# means the general trigger, never a skipped nudge on real pressure.
IS_KING=0
EFFECTIVE_TRIGGER="$GENERAL_TRIGGER"
if [[ -n "$CROWN_LEVEL" ]]; then
    IS_KING=1
    EFFECTIVE_TRIGGER="$KING_TRIGGER"
fi

# ── 5. Band index (shared by both latches). No reading -> band "0" so the ─────
# orphan check can still latch once; the context check is skipped without a reading.
BAND="0"
if [[ -n "$USED_PCT" ]]; then
    BAND=$(( USED_PCT / 10 ))
fi
# Latches are PER-SESSION state (keyed on transcript basename + band, which is
# already collision-free across sessions), so they live in the GLOBAL state dir,
# not a CWD-relative .fno. A CWD-relative latch splits across checkouts: a
# session whose cwd moves between the canonical tree and a worktree (or fires
# from a cwd with no .fno) gets a fresh namespace and re-nudges within the same
# band, defeating the once-per-band anti-nag. Worst case a worktree worker's
# latch vanishes when the worktree archives. Global + transcript-keyed closes it.
LATCH_DIR="${HOME}/.fno"
mkdir -p "$LATCH_DIR" 2>/dev/null || true
CTX_LATCH="${LATCH_DIR}/.context-nudge-ctx-${TBASE}-${BAND}"
ORPHAN_LATCH="${LATCH_DIR}/.context-nudge-orphan-${TBASE}-${BAND}"
FLUSH_LATCH="${LATCH_DIR}/.context-nudge-flush-latch-${TBASE}-${BAND}"
# Static-HEAD tracking is band-independent (a turn-end is a turn-end at any
# pressure), so it keys on the transcript alone, not the band.
FLUSH_STATE="${LATCH_DIR}/.context-nudge-flush-${TBASE}"

REASON=""

# ── 5b. The compact instruction, gated on a MEASURED injection path. ──────────
# `/compact` is a REPL built-in the Skill tool cannot call, so a session that
# needs one either fires it at its own prompt line or asks its operator to type
# it. WHICH of those is possible is not derivable from the session's shape: the
# front door needs a registry row, a keystroke lane, and on the daemon lane a
# roster entry with a resolvable control socket. This hook used to prescribe the
# inject unconditionally; a session with no path ran it twice, got a resolution
# miss twice, read that miss as "the session is busy with my turn", and gave up on
# compacting for the rest of its run. Advice naming a mechanism that cannot fire
# is worse than no advice, so ASK rather than guess.
#
# `--check` is the one resolver answering about itself: it injects nothing and
# resolves through the same path the real send would, so it cannot say yes where
# the send says no. It is asked with `--to-self` (see the note on the function),
# which resolves the caller from its ambient harness markers rather than from the
# payload's SESSION_ID, because the verdict differs by recipient and the prescribed
# command is a self-address too. A contaminated or absent ambient identity makes
# --to-self error, which prints no verdict and lands on the unmeasurable branch.
#
# What it can and cannot promise: a PATH exists. No probe can see whether the
# prompt line is idle, so a send can still come back unconfirmed even on a yes.
#
# Fail direction, matching the rest of this hook: anything unmeasurable (no `fno`,
# an unbuilt binary, a timeout) takes the operator sentence. Telling a session to
# ask its operator when it could have self-injected costs one message; the reverse
# cost a session.
# THREE answers, not two. "I measured no path" and "I could not measure" are
# different claims, and collapsing them would make this hook do the thing it
# exists to stop: state a verdict it did not establish.
compact_instruction() {
    # Branch on the VERDICT the check printed, not on an exit code: a wrapper, a
    # pipeline, or a swallowed status can flip a code, and reading one has already
    # produced false greens elsewhere in this repo today. The word is the signal.
    # Asks with --to-self, the same address the prescribed command uses, because
    # the answer DIFFERS by recipient: the mux pane lane is guarded and refuses a
    # mid-turn recipient, and a session injecting into itself is mid-turn by
    # construction, so a peer-shaped question would answer "path" where the self
    # answer is "none". Absent ambient identity --to-self errors, which prints
    # neither verdict and lands on the unmeasurable branch.
    local out=""
    if command -v fno >/dev/null 2>&1; then
        out=$(with_timeout 5 fno mail send '/compact' --to-self --raw --check 2>/dev/null || true)
    fi
    local _ask="ask your operator to type /compact <brief-path> at your prompt, and say in one line what to preserve"
    if [[ "$out" == injectable:* ]]; then
        printf '%s' "/compact is a REPL built-in your Skill tool cannot call, and this session HAS an injection path (measured just now: ${out}), so fire it at your own prompt line: fno mail send '/compact <brief-path>' --to-self --raw   (the leading slash is load-bearing, and the brief path matters: a bare /compact at exhausted context has no headroom left to summarize). A path is not a landing - if the receipt comes back unconfirmed, read your own prompt before assuming either way, and never re-send."
    elif [[ "$out" == not-injectable:* ]]; then
        printf '%s' "/compact is a REPL built-in your Skill tool cannot call, and this session has NO injection path: 'fno mail send /compact --to-self --raw --check' answered ${out}. That means no registry row, a non-keystroke lane, a guarded mux pane (which refuses a mid-turn recipient, and you are mid-turn), or no control socket to write into. It is NOT a claim that you are dead. You cannot fire the verb yourself, so ${_ask}. On whether /fno-me would help: it adds the registry row this check needs FIRST, and nothing else - it creates no daemon roster entry and no control socket - so it flips this answer only for a session that already had those and merely lacked a row. Read the reason above rather than guessing which case you are."
    else
        printf '%s' "/compact is a REPL built-in your Skill tool cannot call, and whether you can inject it at your own prompt line could not be measured here (no fno on PATH, or the check timed out), so this nudge will not claim you have a path. Either ${_ask}, or check for yourself with 'fno mail send /compact --to-self --raw --check' and fire it if that says injectable."
    fi
}

# ── 6. Check (a): context pressure (EVERY session). Two branches: ─────────────
#   quality  - large window (>= MIN_WINDOW) past the trigger (returns diminish)
#   capacity - any window about to hit the absolute RESERVE floor (preamble-heavy
#              lanes where a percentage measures the preamble, not the work)
# Either sets FIRE_CTX; the once-per-band latch still gates the actual emit, so a
# session hovering at the reserve across bands still nudges once per new band.
FIRE_CTX=0
if [[ -n "$USED_PCT" && -n "$WINDOW_TOKENS" ]]; then
    if [[ "$WINDOW_TOKENS" -ge "$MIN_WINDOW" && "$USED_PCT" -ge "$EFFECTIVE_TRIGGER" ]]; then
        FIRE_CTX=1
    fi
    if [[ -n "$USED_TOKENS" && $(( WINDOW_TOKENS - USED_TOKENS )) -le "$RESERVE" ]]; then
        FIRE_CTX=1
    fi
fi
if [[ "$FIRE_CTX" -eq 1 && ! -f "$CTX_LATCH" ]]; then
    touch "$CTX_LATCH" 2>/dev/null || true
    # Measured ONCE per fire, inside the latch, and shared by both branches: both
    # ask for a compact and the answer does not depend on the crown. Outside the
    # latch this would probe on every Stop.
    _compact_ask="$(compact_instruction)"
    if [[ "$IS_KING" -eq 1 ]]; then
        emit_event "king_context_nudge" \
            "{\"used_pct\":${USED_PCT},\"trigger\":${KING_TRIGGER},\"crown_level\":${CROWN_LEVEL},\"crown_scope\":\"${CROWN_SCOPE}\",\"session_id\":\"${SESSION_ID}\"}"
        # Roll up the king's neighbourhood from the same registry read (no second
        # source). Workers are counted by the orphan check below; peers and the
        # king above derive the same way, so the nudge states the roll-up instead
        # of asking the king to reconstruct it. Superset is approximate (my scope
        # starts with theirs); it degrades to silence, never to a wrong claim.
        PEER_KINGS=$(printf '%s' "$AGENTS_JSON" | jq -r --arg s "$CROWN_SCOPE" \
            '[.agents[] | select((.crown_level // 0) > 0 and (.crown_scope // "") != $s)] | length' 2>/dev/null || printf '%s' 0)
        KING_ABOVE=$(printf '%s' "$AGENTS_JSON" | jq -r --arg s "$CROWN_SCOPE" \
            '[.agents[] | select((.crown_level // 0) > 0 and (.crown_scope // "") != $s and ($s | startswith(.crown_scope // "")))] | length' 2>/dev/null || printf '%s' 0)
        _rollup=""
        [[ "$PEER_KINGS" =~ ^[0-9]+$ && "$PEER_KINGS" -gt 0 ]] && _rollup=" ${PEER_KINGS} peer king(s) also in flight."
        [[ "$KING_ABOVE" =~ ^[0-9]+$ && "$KING_ABOVE" -gt 0 ]] && _rollup="${_rollup} A king above holds your scope."
        # A crown SURVIVES a compact, so this percentage is not a handoff trigger
        # for a king; it is a compact trigger. The nudge says that plainly rather
        # than pointing a king at the more expensive of the two moves. Handoff is a
        # judgement about ruling quality, so it is written as a self-assessment and
        # deliberately carries no number.
        #
        # The rung is NOT printed. Rung semantics changed when succession moved
        # into spawn and pre-existing rows were never migrated, so a stored level
        # is stale for any king stamped before that. The scope is unambiguous and
        # says everything the king needs. The event above still records the stored
        # level: that is a snapshot for whoever migrates the rows, and no session
        # acts on it.
        REASON="context: ${USED_PCT}% used (${USED_TOKENS:-?} of ${WINDOW_TOKENS:-?} tokens). You hold the crown over ${CROWN_SCOPE}. A crown is maintained across a compact - your crown, session id, mail handle, and claims all come out the other side - so the move here is to COMPACT AND KEEP RULING. ${_compact_ask} Handing off is a different decision and this percentage is not its trigger: hand off when your ORCHESTRATION is visibly degrading (you are making worse calls, losing threads, repeating yourself) and a fresh session would rule ${CROWN_SCOPE} better. Ask yourself that about your last few rulings, not about this number. The cost is concrete either way: a successor gets a NEW mail handle, so every worker still holding yours is orphaned at review. If you judge a handoff is right anyway: bash skills/target/scripts/handoff.sh, or spawn your heir over your own scope, which transfers the crown in the same atomic write that vacates yours - 'fno agents spawn -k \"${CROWN_SCOPE}\" \"<seed prompt>\"' - and close this pane only after the successor's session header prints.${_rollup}"
    else
        emit_event "session_context_nudge" \
            "{\"used_pct\":${USED_PCT},\"trigger\":${GENERAL_TRIGGER},\"session_id\":\"${SESSION_ID}\"}"
        # Shape the ask by what already survives a compact. plan_path is read
        # lazily here - only on a real fire, not every Stop - and best-effort: a
        # missing read is the "neither" shape, never a skipped nudge. Reading it
        # for wording (not gating) keeps the arm-handoff invariant intact: a king
        # pass, which has no such manifest, still fires on real pressure above.
        PLAN_PATH=""
        if command -v fno >/dev/null 2>&1; then
            PLAN_PATH=$(with_timeout 3 fno state show --type target --field plan_path 2>/dev/null | head -1 || true)
        fi
        _compact_core="context: ${USED_PCT}% used (${USED_TOKENS:-?} of ${WINDOW_TOKENS:-?} tokens). You are past the session compact trigger (${GENERAL_TRIGGER}%). Returns diminish well before this window fills, so a long run degrades from here. Compact now: your session id, mail handle, and claims all survive it. ${_compact_ask}"
        if [[ -n "$PLAN_PATH" ]]; then
            REASON="${_compact_core} You have a plan bound, so the plan, STATE.md and SUMMARY.md survive the compact - but your in-flight judgment does not. Before you compact, flush what is only in volatile state: commit small fixes in this PR as their own atomic commit, 'fno carveout add -k deferred \"...\"' for separable work, 'fno backlog idea' for new findings, and note any plan drift in SUMMARY.md."
        else
            CANON_DOC=""
            if command -v fno >/dev/null 2>&1; then
                CANON_DOC=$(with_timeout 3 fno paths handoff --session-id "${SESSION_ID}" 2>/dev/null | head -1 || true)
            fi
            if [[ -n "$CANON_DOC" ]]; then
                REASON="${_compact_core} You have no plan and no crown, so nothing about this session's work survives a compact unless you write it down. Before you compact, write a brief canon doc at ${CANON_DOC} - a markdown file with what you are doing, the key decisions, and the open threads - and commit it, so a fresh session or a successor can pick up where you left off. The PreCompact hook keeps that doc's mechanical sections fresh; you fill its merge-order and open-decisions sections."
            else
                REASON="${_compact_core} You have no plan and no crown, so nothing about this session's work survives a compact unless you write it down. Before you compact, write a brief canon doc - a markdown file with what you are doing, the key decisions, and the open threads - and commit it, so a fresh session or a successor can pick up where you left off."
            fi
        fi
    fi
fi

# ── 7. Check (b): orphaned live children (CROWN-ONLY; latches INDEPENDENTLY). ─
if [[ "$IS_KING" -eq 1 && "$ORPHAN_COUNT" -gt 0 && ! -f "$ORPHAN_LATCH" ]]; then
    # Resolution 3: a carveout carrying THIS scope (structured field, not free
    # text) means the king stated the orphaning and fell back to advisory
    # self-review. Scope match is the discriminator, or any carveout silences it.
    RESOLVED=0
    if command -v fno >/dev/null 2>&1; then
        # --all is load-bearing: `carveout list` now scopes to the current
        # session by default, and this check must see a carveout filed by ANY
        # session (the king that stated the orphaning is usually not this one).
        CARVEOUTS=$(with_timeout 3 fno carveout list --all --json 2>/dev/null || true)
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
            "{\"crown_level\":${CROWN_LEVEL},\"crown_scope\":\"${CROWN_SCOPE}\",\"workers\":\"${ORPHANS}\",\"count\":${ORPHAN_COUNT},\"session_id\":\"${SESSION_ID}\"}"
        ORPHAN_REASON="You hold the crown over ${CROWN_SCOPE} and ${ORPHAN_COUNT} worker(s) you spawned are still live (${ORPHANS}). A reign that spawns workers cannot be a pure pass: abdicating now leaves them with nobody to mail when they reach review. Pick one and act, then this stops: (1) stay as court through the wave; (2) hand the crown to an heir by spawning it over your own scope, which vacates yours in the same atomic write - 'fno agents spawn -k \"${CROWN_SCOPE}\" \"<seed prompt>\"'; (3) record that these workers are review-orphaned with 'fno carveout add -k deferred --scope ${CROWN_SCOPE} \"...\"' and they fall back to advisory self-review."
        if [[ -n "$REASON" ]]; then
            REASON="${REASON}  ||  ${ORPHAN_REASON}"
        else
            REASON="$ORPHAN_REASON"
        fi
    fi
fi

# ── 7b. Check (c): flush-cadence (work only in volatile state). ───────────────
# Carveouts, filed nodes, PR bodies, commits and SUMMARY.md are written at the
# END of a unit of work; a session that never reaches the end records nothing
# (a reign can die mid-flight holding two PRs, unpushed, in a local worktree).
# git is only the code-vertical detector; the rule is vertical-agnostic - work
# in volatile state moves to a durable store - so the check degrades to silence
# when there is no repo (an active epic runs growth, marketing, finance and ops
# with no git tree). Reports a FILE count, never a commit count: "0 commits this
# session" reads as "nothing happened" when it means "everything is still in the
# tree" - the empty-result trap in its most dangerous form.
FIRE_FLUSH=0
FLUSH_DIRTY=0
FLUSH_STATIC=0
_porcelain=$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null || true)
if [[ -n "$_porcelain" ]]; then
    FLUSH_DIRTY=$(printf '%s' "$_porcelain" | grep -c . 2>/dev/null || printf '%s' 0)
    case "$FLUSH_DIRTY" in ''|*[!0-9]*) FLUSH_DIRTY=0 ;; esac
fi
_head=$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || true)
# Count consecutive Stops that saw the same HEAD. No HEAD (no commits / no git)
# -> can't measure staleness -> skip, degrade to silence.
if [[ -n "$_head" ]]; then
    _stored=""
    [[ -f "$FLUSH_STATE" ]] && _stored=$(cat "$FLUSH_STATE" 2>/dev/null || true)
    _stored_head=""; _stored_count=0
    if [[ "$_stored" == *' '* ]]; then
        _stored_head="${_stored%% *}"
        _sc="${_stored#* }"
        case "$_sc" in ''|*[!0-9]*) ;; *) _stored_count="$_sc" ;; esac
    fi
    if [[ "$_head" == "$_stored_head" ]]; then
        FLUSH_STATIC=$((_stored_count + 1))
    else
        FLUSH_STATIC=1
    fi
    printf '%s %s\n' "$_head" "$FLUSH_STATIC" > "$FLUSH_STATE" 2>/dev/null || true
    if [[ "$FLUSH_DIRTY" -gt "$FLUSH_FILE_THRESHOLD" \
        && "$FLUSH_STATIC" -ge "$FLUSH_STATIC_STOPS" && ! -f "$FLUSH_LATCH" ]]; then
        FIRE_FLUSH=1
    fi
fi
if [[ "$FIRE_FLUSH" -eq 1 ]]; then
    touch "$FLUSH_LATCH" 2>/dev/null || true
    emit_event "context_flush_nudge" \
        "{\"dirty_files\":${FLUSH_DIRTY},\"static_stops\":${FLUSH_STATIC},\"session_id\":\"${SESSION_ID}\"}"
    FLUSH_REASON="flush: ${FLUSH_DIRTY} files have uncommitted changes and the tree has not moved in ${FLUSH_STATIC} turn-ends, so this work exists only in this session's volatile state - a compact or a crashed pane loses it. Fix what you found if it is small: commit it now, in this PR, as its own atomic commit. File a carveout ONLY for what is genuinely separable: 'fno carveout add -k deferred \"...\"'. A session that never reaches the end of a unit of work records nothing."
    if [[ -n "$REASON" ]]; then
        REASON="${REASON}  ||  ${FLUSH_REASON}"
    else
        REASON="$FLUSH_REASON"
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
