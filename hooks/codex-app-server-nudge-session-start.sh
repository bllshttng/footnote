#!/usr/bin/env bash
# Codex SessionStart hook: nudge toward the app-server daemon when its control
# socket is absent. Live mail to a codex session is delivered over that socket
# (crates/fno-agents/src/codex_inject.rs); without the daemon it demotes to
# durable. One advisory line; goes silent the moment the socket exists, the same
# self-extinguishing shape as setup-nudge-session-start.sh. Stdout becomes
# session context.

set -uo pipefail

CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"
SOCKET="$CODEX_HOME/app-server-control/app-server-control.sock"

# Silent once the daemon is running: the socket is its marker.
if [[ -S "$SOCKET" ]]; then
  exit 0
fi

cat <<'EOF'
## Codex app-server daemon

No codex app-server control socket found, so live mail to this session will demote to durable. Start the daemon BEFORE launching codex: `codex app-server daemon start` (or `codex app-server daemon bootstrap` for durable SSH-driven use), then RESTART this session. A codex TUI launched before the daemon exists cannot receive live mail without a restart.
EOF
