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
# Provider coverage: claude is fully wired (CLAUDE_SESSION_ID is exported at
# SessionStart). codex/gemini reuse the same provider-agnostic entry point,
# but their harnesses do not yet expose a SessionStart hook + session-id env
# (design Open Question 5); the lookups below are best-effort and no-op when
# the env var is absent, so this hook is correct today and ready when they do.
set -euo pipefail

REPO_ROOT="${CLAUDE_PROJECT_DIR:-${GEMINI_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}}"
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI_DIR="$(cd "$HOOK_DIR/.." && pwd)/cli"

# Detect the harness and its session-id env in lockstep (mirrors
# session-start.sh detect_platform). Only the matched provider's id is read.
if [[ -n "${GEMINI_PROJECT_DIR:-}" ]]; then
    PROVIDER="gemini"; SESSION_ID="${GEMINI_SESSION_ID:-}"
elif [[ -n "${CODEX_PLUGIN_ROOT:-}" ]]; then
    PROVIDER="codex"; SESSION_ID="${CODEX_THREAD_ID:-${CODEX_SESSION_ID:-}}"
elif [[ -n "${CLAUDE_PLUGIN_ROOT:-}" ]]; then
    PROVIDER="claude"; SESSION_ID="${CLAUDE_SESSION_ID:-}"
else
    exit 0  # generic/unknown harness: nothing addressable to register
fi

# Nothing to register without a session id (the entry point also guards this).
[[ -n "$SESSION_ID" ]] || exit 0

cd "$REPO_ROOT" 2>/dev/null || true
uv run --project "$CLI_DIR" python3 -m fno.agents.register_session \
    --provider "$PROVIDER" \
    --session-id "$SESSION_ID" \
    --cwd "$REPO_ROOT" 2>/dev/null || true

exit 0
