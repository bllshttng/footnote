#!/usr/bin/env bash
# SessionStart hook (US7): register an operator-started session in the agent
# registry so peers can `fno mail send` to it by name. A session a human
# started by hand has no spawn/host registry row; this hook creates one.
#
# Hook contract: NEVER blocks session start. The registration is fail-open
# (`|| true`, exit 0 always) and the Python entry point itself swallows any
# error into a `session_register_failed` event (AC7-ERR). stdout stays empty
# so this hook contributes nothing to the session preamble.
#
# Provider coverage: Claude wires this hook directly. Codex's shared
# session-start wrapper invokes it once with CODEX_PLUGIN_ROOT hydrated, so the
# durable CODEX_THREAD_ID is addressable through fno mail. Gemini remains
# best-effort and no-ops when its session-id environment is absent.
set -euo pipefail

REPO_ROOT="${CLAUDE_PROJECT_DIR:-${GEMINI_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}}"
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI_DIR="$(cd "$HOOK_DIR/.." && pwd)/cli"

# Auto-join is opt-in (config.agents.auto_register_sessions, default false): a
# session joins the roster deliberately via `/fno-me` (`fno agents register`),
# so the roster stays the workers you coordinate with, not every terminal. Flip
# the knob to auto-join every hand-started session. A failed/absent read is the
# default (false) — never auto-register on a config we could not confirm.
AUTO="$(fno config get agents.auto_register_sessions 2>/dev/null || true)"
[[ "$AUTO" == "true" ]] || exit 0

# Detect the harness and read the SAME session-id env the rest of fno resolves
# on (harness_identity.HARNESS_SESSION_MARKERS): claude uses CLAUDE_CODE_SESSION_ID,
# not CLAUDE_SESSION_ID (the old name here was unset, so claude never registered).
if [[ -n "${GEMINI_PROJECT_DIR:-}" ]]; then
    HARNESS="gemini"; SESSION_ID="${GEMINI_SESSION_ID:-}"
elif [[ -n "${CODEX_PLUGIN_ROOT:-}" ]]; then
    HARNESS="codex"; SESSION_ID="${CODEX_THREAD_ID:-${CODEX_SESSION_ID:-}}"
elif [[ -n "${CLAUDE_PLUGIN_ROOT:-}" ]]; then
    HARNESS="claude"; SESSION_ID="${CLAUDE_CODE_SESSION_ID:-}"
else
    exit 0  # generic/unknown harness: nothing addressable to register
fi

# Nothing to register without a session id (the entry point also guards this).
[[ -n "$SESSION_ID" ]] || exit 0

cd "$REPO_ROOT" 2>/dev/null || true
uv run --project "$CLI_DIR" python3 -m fno.agents.register_session \
    --harness "$HARNESS" \
    --session-id "$SESSION_ID" \
    --cwd "$REPO_ROOT" 2>/dev/null || true

exit 0
