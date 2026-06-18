#!/usr/bin/env bash
# Spawn a subagent using the current driver's native primitive.
#
# Usage:
#   spawn-subagent.sh "task prompt" [--model MODEL] [--driver DRIVER]
#
# Exit codes:
#   0   subagent completed (stdout contains its result)
#   2   driver disallows shell-level spawn (use the driver's tool instead)
#   77  driver CLI not on PATH
#   1+  subagent failed
#
# Driver resolution:
#   1. --driver flag
#   2. $FNO_DRIVER env var
#   3. $CLAUDECODE_SESSION_ID -> claude-code
#   4. hermes-agent on PATH + $HERMES_SESSION_ID or ~/.hermes/config.yaml -> hermes
#   5. openclaw on PATH -> openclaw
#   6. fallback -> error

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  cat <<'EOF'
Usage: spawn-subagent.sh "task prompt" [--model MODEL] [--driver DRIVER]

Spawn a subagent under the current driver's native primitive.

Options:
  --model MODEL   Override the subagent's model (driver must support it)
  --driver NAME   claude-code | hermes | openclaw (auto-detects by default)
  -h, --help      Show this help

Exit codes:
  0   success
  2   driver disallows shell-level spawn (use the driver's native tool)
  77  driver CLI not on PATH
  1   subagent failed
EOF
}

PROMPT=""
MODEL=""
DRIVER="${FNO_DRIVER:-}"

# Positional: the first non-flag arg is the prompt.
while [[ $# -gt 0 ]]; do
  case "$1" in
    --model)  MODEL="$2"; shift 2 ;;
    --driver) DRIVER="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    --) shift; break ;;
    -*)
      echo "unknown flag: $1" >&2
      exit 2
      ;;
    *)
      PROMPT="$1"
      shift
      ;;
  esac
done

if [[ -z "$PROMPT" ]]; then
  echo "ERROR: no prompt provided" >&2
  usage >&2
  exit 2
fi

# Driver auto-detect if not explicitly set
if [[ -z "$DRIVER" ]]; then
  if [[ -n "${CLAUDECODE_SESSION_ID:-}" ]]; then
    DRIVER="claude-code"
  elif [[ -n "${HERMES_SESSION_ID:-}" ]] || \
       { [[ -f "$HOME/.hermes/config.yaml" ]] && command -v hermes-agent &>/dev/null; }; then
    DRIVER="hermes"
  elif command -v openclaw &>/dev/null; then
    DRIVER="openclaw"
  else
    echo "ERROR: cannot auto-detect driver; pass --driver explicitly" >&2
    exit 2
  fi
fi

case "$DRIVER" in
  claude-code)
    # Claude Code dispatches subagents via the Agent tool inside a session.
    # A shell-level spawn would lose the parent's conversation context, so
    # this helper refuses and tells the caller to use the Agent tool path.
    echo "ERROR: under claude-code, use the Agent tool directly - shell spawn is not supported." >&2
    exit 2
    ;;
  hermes)
    cli="${HERMES_CLI:-hermes-agent}"
    if ! command -v "$cli" &>/dev/null; then
      exit 77
    fi
    # Build argv as two branches to avoid bash 3.2's set -u empty-array
    # bug (same workaround as driver-hermes.sh / driver-openclaw.sh).
    # Hermes preferred path inside a session is the delegate_task tool;
    # from shell we spawn a fresh hermes process per provider-adapters.md.
    if [[ -n "$MODEL" ]]; then
      "$cli" -p "$PROMPT" --model "$MODEL"
    else
      "$cli" -p "$PROMPT"
    fi
    ;;
  openclaw)
    cli="${OPENCLAW_CLI:-openclaw}"
    if ! command -v "$cli" &>/dev/null; then
      exit 77
    fi
    if [[ -n "$MODEL" ]]; then
      "$cli" -p "$PROMPT" --model "$MODEL"
    else
      "$cli" -p "$PROMPT"
    fi
    ;;
  gemini|codex)
    # Existing adapter patterns live in docs/providers/provider-adapters.md.
    # This helper is not the recommended path for these drivers; they have
    # their own in-session spawn primitives.
    echo "ERROR: driver '$DRIVER' uses an in-session spawn tool - see provider-adapters.md" >&2
    exit 2
    ;;
  *)
    echo "ERROR: unknown driver '$DRIVER'" >&2
    exit 2
    ;;
esac
