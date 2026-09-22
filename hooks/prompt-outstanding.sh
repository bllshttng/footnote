#!/usr/bin/env bash
# The open list where the user types: a UserPromptSubmit hook reading the
# daemon's projection cache. The skip rules (mail envelope, machine-shaped
# prompts) live in `fno-agents hook prompt`; a wrapped or machine turn
# renders nothing.
set -uo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH
command -v fno-agents >/dev/null 2>&1 || exit 0
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../scripts/lib/with-timeout.sh
source "$HOOK_DIR/../scripts/lib/with-timeout.sh" 2>/dev/null || exit 0

input="$(cat 2>/dev/null || true)"
[[ -n "$input" ]] || exit 0
printf '%s' "$input" | with_timeout 2 fno-agents hook prompt 2>/dev/null
exit 0
