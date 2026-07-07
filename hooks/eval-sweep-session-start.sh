#!/usr/bin/env bash
# SessionStart hook: kick off a daily-throttled eval-loop ignition in the
# background (observer sweep -> skill-diff tick), then exit instantly.
#
# The eval corpus (.fno/events.jsonl, ~/.fno/ledger.json) is per-machine
# gitignored state, so the sweep must run where the data lives - the developer's
# machine, at session start - not in a stateless CI runner that would sweep an
# empty corpus forever. Fire-only, no render step: unlike reconcile, eval output
# is a background log, not a session-start reminder.
#
# Hook contract: stdout is appended to the session prompt (this hook prints
# nothing); exit 0 = no error. NEVER blocks session start - the sweep+tick are
# detached (see scripts/lib/eval-sweep-throttle.sh).
set -euo pipefail

REPO_ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=scripts/lib/eval-sweep-throttle.sh
source "$HOOK_DIR/../scripts/lib/eval-sweep-throttle.sh" 2>/dev/null || exit 0

eval_sweep_maybe_fire "$REPO_ROOT" || true

exit 0
