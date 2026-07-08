#!/usr/bin/env bash
# SessionStart hook: surface the PRIOR `fno backlog reconcile` sweep as a
# system reminder, then kick off a fresh throttled reconcile in the background.
#
# Hook contract: stdout is appended to the session prompt; exit 0 = no error.
# This hook NEVER blocks session start — the reconcile itself is detached (see
# scripts/lib/reconcile-throttle.sh). The render step is a cheap file read of
# the last sweep's result, so the reminder is always one sweep behind, which is
# the point: session start stays instant.
set -euo pipefail

REPO_ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=scripts/lib/reconcile-throttle.sh
source "$HOOK_DIR/../scripts/lib/reconcile-throttle.sh" 2>/dev/null || exit 0

RESULT="$REPO_ROOT/.fno/.reconcile-result.json"

# 1. Render the prior sweep's result, exactly once. Only surface when the sweep
#    actually closed a drifted node — an empty sweep is silent to avoid noise.
#    Consume-after-show (mv to .shown) so the same result is never re-surfaced
#    across multiple sessions; the next sweep overwrites RESULT with fresh data.
if [[ -f "$RESULT" ]] && command -v jq >/dev/null 2>&1; then
    closed_n=$(jq '.closed | length' "$RESULT" 2>/dev/null || echo 0)
    if [[ "$closed_n" =~ ^[0-9]+$ ]] && (( closed_n > 0 )); then
        nodes=$(jq -r '.closed[].node_id' "$RESULT" 2>/dev/null | paste -sd, - 2>/dev/null)
        echo "reconcile: last sweep closed ${closed_n} drifted node(s) whose PR merged outside the ship gate (${nodes}). Retro sentinels were written; the background harvest files follow-ups from them (or run \`fno retro run\` now)."
    fi
    mv -f "$RESULT" "$RESULT.shown" 2>/dev/null || true
fi

# 1b. Advisory: surface retro-pending sentinels still awaiting harvest. This is
#     the recovery-visibility line for a web-UI merge - the detached job in step
#     2 actually consumes them; this only reports the backlog so it never goes
#     silent between throttle windows. Best-effort + cosmetic: a failed harvest
#     retains its sentinel, so the count re-surfaces next session.
# ponytail: default state dir (env-injectable for tests); a
# config.paths.retro_pending_dir override degrades this count, NOT the harvest -
# `fno retro run` in step 2 resolves the dir via Python and honors the override.
RETRO_PENDING_DIR="${RETRO_PENDING_DIR:-$HOME/.fno/retro-pending}"
if [[ -d "$RETRO_PENDING_DIR" ]]; then
    # Fail-open inside the substitution: under this hook's `set -euo pipefail`, a
    # non-zero `find` (permission race, dir removed after the -d check) would
    # otherwise abort the hook BEFORE the load-bearing reconcile_maybe_fire below.
    # A cosmetic advisory must never kill the reconcile trigger. (gemini review)
    pending_n=$( (find "$RETRO_PENDING_DIR" -maxdepth 1 -name '*.json' -type f 2>/dev/null || true) | wc -l | tr -d ' ')
    if [[ "$pending_n" =~ ^[0-9]+$ ]] && (( pending_n > 0 )); then
        echo "retro: ${pending_n} sentinel(s) pending harvest; the background job harvests them, or run \`fno retro run\`."
    fi
fi

# 1c. Advisory: a dead pr-watch daemon (enabled but not ticking) once ran silent
#     for 18h. Surface it here, same posture as the reconcile line above. The
#     verdict verb self-gates - a disabled install reports verdict=disabled - so
#     we speak only on `dead`. Best-effort; never blocks session start.
if command -v fno >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
    pw_json="$(fno pr-watch status --json 2>/dev/null || true)"
    if [[ -n "$pw_json" ]]; then
        pw_verdict="$(printf '%s' "$pw_json" | jq -r '.verdict // empty' 2>/dev/null || true)"
        if [[ "$pw_verdict" == "dead" ]]; then
            pw_detail="$(printf '%s' "$pw_json" | jq -r '.detail // ""' 2>/dev/null || true)"
            echo "pr-watch: dead (${pw_detail}); run: fno pr-watch install"
        fi
    fi
fi

# 2. Kick off a fresh throttled reconcile (mutate mode, detached). Never blocks.
reconcile_maybe_fire "$REPO_ROOT" || true

exit 0
