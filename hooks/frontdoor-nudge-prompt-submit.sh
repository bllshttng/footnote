#!/usr/bin/env bash
# fno hook: UserPromptSubmit - frontdoor nudge prompt submit
# SessionStart does not fire in the session that installed the plugin, so the
# first prompt after /plugin install is the first chance to start the CLI
# installer. The SessionStart hook (frontdoor-nudge-session-start.sh) keeps the
# same job for every later session; this wrapper adds the same-session prompt.
# Per-prompt cost is bounded by three cheap guards ahead of the full hook: the
# version stamp (installer already succeeded for this version), the announced
# marker (this trigger already surfaced its note for this version), and
# `command -v fno` (something named fno is on PATH, so the mux-probe verdict
# stays the SessionStart hook's per-session job - a per-prompt probe could pay
# its 3s wedge cap on every prompt). SILENT unless it actually surfaces a note:
# UserPromptSubmit exit-0 stdout is added to model context (not user chat), so
# per-prompt chatter is context cost, and this note relies on the model
# relaying it - the same plain-text convention as the SessionStart hook.

set -uo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ROOT="$(cd "$HOOK_DIR/.." && pwd)"
# Same dir, stamp and lock as the SessionStart hook: a Codex session carries no
# CLAUDE_PLUGIN_DATA, so the data dir falls back to the XDG state dir.
DATA="${CLAUDE_PLUGIN_DATA:-${XDG_STATE_HOME:-$HOME/.local/state}/fno/plugin-install}"
STAMP="$DATA/postinstall.version"
MARKER="$DATA/postinstall.announced"
VERSION="$(sed -n -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' "$PLUGIN_ROOT/.claude-plugin/plugin.json" 2>/dev/null | head -1)"

if [[ -n "$VERSION" ]]; then
  [[ "$(cat "$STAMP" 2>/dev/null)" == "$VERSION" ]] && exit 0
  [[ "$(cat "$MARKER" 2>/dev/null)" == "$VERSION" ]] && exit 0
fi
command -v fno >/dev/null 2>&1 && exit 0

# The full hook owns the lock and the detached spawn; its output is the note.
# It never waits on the installer (every descriptor is redirected), so the
# capture here cannot be held open by the background job. Only its actionable
# notes are relayed (both substrings are pinned by the hooks test): when the
# hook falls to its static reminder - no version, or no installer in the
# plugin tree - nothing here can mark the note as surfaced, so relaying it
# would repeat the same lines on every prompt; the using-fno preamble owns
# that story instead.
out="$(bash "$HOOK_DIR/frontdoor-nudge-session-start.sh" 2>/dev/null)"
if [[ "$out" == *postinstall.log* || "$out" == *"install in progress"* ]]; then
  printf '%s\n' "$out"
  [[ -n "$VERSION" ]] && printf '%s' "$VERSION" >"$MARKER" 2>/dev/null
fi
exit 0
