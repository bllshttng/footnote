#!/usr/bin/env bash
# UserPromptSubmit hook: surface unread inbox threads as a system reminder.
# Hook contract: stdout is appended to the next assistant turn; exit 0 = no error.
set -euo pipefail

REPO_ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"

# CLI_DIR: the abilities cli package, always co-located with this hook script.
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI_DIR="$(cd "$HOOK_DIR/.." && pwd)/cli"

# Single subprocess: drain question wake-signals + scan unread threads +
# render the entire system-reminder. Replaces the prior six-`uv run` chain
# (Gemini MEDIUM finding on PR #225 about subprocess startup overhead).
# A non-zero rc or empty output means there is nothing to surface.
cd "$REPO_ROOT" 2>/dev/null || true
uv run --project "$CLI_DIR" python3 -m fno.inbox.unread_scan \
    wake-render "$REPO_ROOT" "new question(s) since your last turn" 2>/dev/null || true
