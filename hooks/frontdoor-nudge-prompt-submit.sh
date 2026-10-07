#!/usr/bin/env bash
# fno hook: UserPromptSubmit - frontdoor nudge prompt submit
# SessionStart does not fire in the session that installed the plugin, so the
# first prompt after /plugin install is the first chance to surface the install
# notice. The SessionStart hook (frontdoor-nudge-session-start.sh) keeps the
# same job for every later session; this wrapper adds the same-session prompt.
# The notice starts nothing: it asks the agent to get the user's yes, then run
# postinstall.sh in the open. Per-prompt cost is bounded by two cheap
# guards ahead of the full hook: the announced marker (this trigger already
# surfaced its notice for this version) and the fno-bin resolver (something
# named fno resolves, so the mux-probe verdict stays the SessionStart hook's
# per-session job - a per-prompt probe could pay its 3s wedge cap on every
# prompt). SILENT unless it relays the notice: UserPromptSubmit exit-0 stdout
# is added to model context (not user chat), so per-prompt chatter is context
# cost, and this notice relies on the model relaying it - the same plain-text
# convention as the SessionStart hook.

set -uo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The per-turn guard (tests/hooks/test_with_timeout.sh) requires every
# UserPromptSubmit hook that names a daemon binary to carry the shared bound,
# so the file sources it even though its own guards are pure builtins.
source "$HOOK_DIR/../scripts/lib/with-timeout.sh" 2>/dev/null || exit 0
source "$HOOK_DIR/lib/fno-bin.sh" 2>/dev/null || true
PLUGIN_ROOT="$(cd "$HOOK_DIR/.." && pwd)"
# Same dir and marker as the SessionStart hook's data dir: a Codex session
# carries no CLAUDE_PLUGIN_DATA, so it falls back to the XDG state dir. The
# install stamp is gone with the auto-install: the door probe, not a stamp,
# decides whether an install is still missing.
DATA="${CLAUDE_PLUGIN_DATA:-${XDG_STATE_HOME:-$HOME/.local/state}/fno/plugin-install}"
MARKER="$DATA/postinstall.announced"
VERSION="$(sed -n -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' "$PLUGIN_ROOT/.claude-plugin/plugin.json" 2>/dev/null | head -1)"

if [[ -n "$VERSION" ]]; then
  [[ "$(cat "$MARKER" 2>/dev/null)" == "$VERSION" ]] && exit 0
fi

# The one run-once setup attempt: when the CLI resolves, run the setup verb
# once per install, backgrounded so the prompt never waits on it (the log
# lands beside this hook's own markers). The verb reads no stdin on a
# non-TTY, stops at its own done markers, and one attempted-marker here caps
# the respawn even when the run ends needs-human.
if [[ -n "$(fno_bin)" ]] && [[ ! -f "$DATA/setup.attempted" ]] \
  && [[ ! -f "$HOME/.fno/sidecar/setup-done" ]]; then
  mkdir -p "$DATA" 2>/dev/null || true
  : >"$DATA/setup.attempted" 2>/dev/null || true
  "$(fno_bin)" config setup run --once --json >>"$DATA/setup.log" 2>&1 &
fi
[[ -n "$(fno_bin)" ]] && exit 0

# The full hook prints the consent notice and starts nothing, so the capture
# here cannot be held open by a background job. Relay it once per version:
# the marker caps the repeat, whatever note the hook printed. The hook no
# longer creates the data dir (it starts nothing), so the marker write makes
# it - best effort, same posture as the guarded write.
out="$(bash "$HOOK_DIR/frontdoor-nudge-session-start.sh" 2>/dev/null)"
if [[ -n "$out" ]]; then
  printf '%s\n' "$out"
  mkdir -p "$DATA" 2>/dev/null || true
  [[ -n "$VERSION" ]] && printf '%s' "$VERSION" >"$MARKER" 2>/dev/null
fi
exit 0
