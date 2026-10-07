#!/usr/bin/env bash
# fno hook: SessionStart - reconcile session start
# SessionStart hook: kick off a fresh throttled reconcile in the background.
#
# Hook contract: stdout is appended to the session prompt; exit 0 = no error.
# This hook NEVER blocks session start — the reconcile itself is detached (see
# scripts/lib/reconcile-throttle.sh). The sweep's warnings no longer render
# here: the notice_route daemon arm routes them to the owning lead (x-f455),
# so session start stays instant and the warnings reach one owner instead of
# every session.
set -euo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

REPO_ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# HOME-as-repo guard (law d-8ddaba56): when the cwd is $HOME outside git, the
# pwd fallback makes <repo>/.fno IS the state root, and the sweep would write
# result stamps at the top level of the state root. Skip the repo-space work:
# the sweep belongs to a checkout, and the state root is not one.
_git_toplevel="$(git -C "$REPO_ROOT" rev-parse --show-toplevel 2>/dev/null || true)"
_state_dir_phys="$(cd "${FNO_HOME:-$HOME/.fno}" 2>/dev/null && pwd -P || true)"
_repo_fno_phys="$(cd "$REPO_ROOT/.fno" 2>/dev/null && pwd -P || true)"
if [[ -z "$_git_toplevel" ]] \
    || [[ -n "$_state_dir_phys" && "$_repo_fno_phys" == "$_state_dir_phys" ]]; then
    exit 0
fi

# shellcheck source=scripts/lib/reconcile-throttle.sh
source "$HOOK_DIR/../scripts/lib/reconcile-throttle.sh" 2>/dev/null || exit 0

# The reconcile render block is gone (x-f455 change 3): the promise-gate,
# canonical-sync and orphan-plan warnings are lead-scope context, and the
# notice_route daemon arm routes them to the owning lead as one deduped
# mail, renaming each consumed result file to `.shown` itself. This hook
# keeps only the trigger duty: the pr-watch heal (1c) and the throttled
# reconcile fire below.

# 1b. Advisory: surface retro-pending sentinels still awaiting harvest. This is
#     the recovery-visibility line for a web-UI merge - the detached job in step
#     2 actually consumes them; this only reports the backlog so it never goes
#     silent between throttle windows. Best-effort + cosmetic: a failed harvest
#     retains its sentinel, so the count re-surfaces next session.
# ponytail: default state dir (env-injectable for tests); a
# config.paths.retro_pending_dir override degrades this count, NOT the harvest -
# `fno backlog retro run` in step 2 resolves the dir via Python and honors the override.
RETRO_PENDING_DIR="${RETRO_PENDING_DIR:-$HOME/.fno/retro-pending}"
if [[ -d "$RETRO_PENDING_DIR" ]]; then
    # Fail-open inside the substitution: under this hook's `set -euo pipefail`, a
    # non-zero `find` (permission race, dir removed after the -d check) would
    # otherwise abort the hook BEFORE the load-bearing reconcile_maybe_fire below.
    # A cosmetic advisory must never kill the reconcile trigger. (gemini review)
    pending_n=$( (find "$RETRO_PENDING_DIR" -maxdepth 1 -name '*.json' -type f 2>/dev/null || true) | wc -l | tr -d ' ')
    if [[ "$pending_n" =~ ^[0-9]+$ ]] && (( pending_n > 0 )); then
        echo "retro: ${pending_n} sentinel(s) pending harvest; the background job harvests them, or run \`fno backlog retro run\`."
    fi
fi

# 1c. Self-heal: a dead pr-watch daemon (enabled but not ticking) once ran
#     silent for 18h. On `dead` we now fire `fno do pr watch heal` (enabled-gated,
#     claim single-flighted, detached so session start never waits on launchctl)
#     instead of only advising. A `wedged` verdict (ticking, every tick failing)
#     also takes heal: it re-renders through refresh_watcher and defers while a
#     tick holds the claim, so the cure cannot kill a live tick .
#     Best-effort; never blocks session start.
if command -v fno >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
    if pw_json="$(fno do pr watch status --json 2>/dev/null || true)" && [[ -n "$pw_json" ]]; then
        pw_verdict="$(printf '%s' "$pw_json" | jq -r '.verdict // empty' 2>/dev/null || true)"
        if [[ "$pw_verdict" == "dead" ]]; then
            pw_detail="$(printf '%s' "$pw_json" | jq -r '.detail // ""' 2>/dev/null || true)"
            echo "pr-watch: dead (${pw_detail}); self-heal started (fno do pr watch heal)"
            (fno do pr watch heal >/dev/null 2>&1 &)
        elif [[ "$pw_verdict" == "wedged" ]]; then
            pw_detail="$(printf '%s' "$pw_json" | jq -r '.detail // ""' 2>/dev/null || true)"
            echo "pr-watch: wedged (${pw_detail}); self-heal started (fno do pr watch heal)"
            (fno do pr watch heal >/dev/null 2>&1 &)
        fi
    fi
fi

# 2. Kick off a fresh throttled reconcile (mutate mode, detached). Never blocks.
reconcile_maybe_fire "$REPO_ROOT" || true

exit 0
