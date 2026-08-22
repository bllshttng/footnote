#!/usr/bin/env bash
# SessionStart hook (US7): register an operator-started session in the agent
# registry so peers can `fno agents mail send` to it by name. A session a human
# started by hand has no spawn/host registry row; this hook creates one.
#
# Hook contract: NEVER blocks session start. The registration is fail-open
# (`|| true`, exit 0 always) and the Python entry point itself swallows any
# error into a `session_register_failed` event (AC7-ERR). stdout stays empty
# so this hook contributes nothing to the session preamble.
#
# Provider coverage: Claude wires this hook directly. Codex's shared
# session-start wrapper invokes it once with CODEX_PLUGIN_ROOT hydrated, so the
# durable CODEX_THREAD_ID is addressable through fno agents mail. Gemini remains
# best-effort and no-ops when its session-id environment is absent.
set -euo pipefail

REPO_ROOT="${CLAUDE_PROJECT_DIR:-${GEMINI_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}}"
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI_DIR="$(cd "$HOOK_DIR/.." && pwd)/cli"

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

ARGS=(--harness "$HARNESS" --session-id "$SESSION_ID" --cwd "$REPO_ROOT")

# A footnote-SPAWNED worker (FNO_AGENT_SELF = its own row name) restamps rather
# than registers: the session id we passed at spawn is not durable (claude has
# been seen continuing under a different uuid ~35s in) while the row NAME is,
# so the name is the key the correction lands on. Runs at any knob setting: the
# opt-in below governs whether a hand-started terminal JOINS the roster (default
# false; `/fno-me` is the deliberate join), and a spawned worker is already on it.
if [[ -n "${FNO_AGENT_SELF:-}" ]]; then
    ARGS+=(--agent-self "$FNO_AGENT_SELF")
elif [[ "$(fno config get agents.auto_register_sessions 2>/dev/null || true)" != "true" ]]; then
    exit 0
fi

uv run --project "$CLI_DIR" python3 -m fno.agents.register_session "${ARGS[@]}" 2>/dev/null || true

exit 0
