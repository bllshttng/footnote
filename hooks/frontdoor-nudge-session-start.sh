#!/usr/bin/env bash
# SessionStart hook: when the `fno` Rust front door is not active on PATH, start
# the plugin installer (.claude-plugin/postinstall.sh) or remind the user to
# install it. Claude Code has no plugin install hook, so this is the only place a
# plugin install runs the installer. It runs detached, once per plugin version,
# under a lock, and logs to ${CLAUDE_PLUGIN_DATA}/postinstall.log. Goes SILENT the
# moment the front door is active. Stdout becomes session context (same
# plain-text convention as setup-nudge-session-start.sh).

set -uo pipefail

# The Rust front door answers a mux-only verb; the Python `fno-py` has no `mux`
# subcommand and fails "No such command". This is the same probe `fno doctor`'s
# `_probe_is_mux` uses. `fno mux ls --json` is read-only, returns `[]` with no
# server, and does not need the daemon, so it is fast. Bound it anyway so a
# wedged socket can never stall session start.
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/with-timeout.sh
source "$HOOK_DIR/../scripts/lib/with-timeout.sh" 2>/dev/null || exit 0

if command -v fno >/dev/null 2>&1; then
  with_timeout 3 fno mux ls --json >/dev/null 2>&1
  probe_rc=$?
  # 124 means our own bound fired. A wedged socket still PROVES the Rust front
  # door is present: `fno-py` has no `mux` verb and fails fast with a usage
  # error, it cannot hang here. Treating that as "not the front door" would nag a
  # user to install what they already have, every session, for as long as the
  # socket stays wedged. Now that the cap actually fires on every host rather
  # than only where Homebrew supplied timeout(1), that misread is reachable
  # everywhere, so it has to be distinguished from a real probe failure.
  if [[ $probe_rc -eq 0 || $probe_rc -eq 124 ]]; then
    exit 0 # `fno` on PATH IS the Rust mux front door - nothing to remind
  fi
fi

PLUGIN_ROOT="$(cd "$HOOK_DIR/.." && pwd)"
INSTALLER="$PLUGIN_ROOT/.claude-plugin/postinstall.sh"
DATA="${CLAUDE_PLUGIN_DATA:-}"
STAMPED_LOG=""
if [[ -n "$DATA" && -f "$INSTALLER" ]] && mkdir -p "$DATA" 2>/dev/null; then
  LOG="$DATA/postinstall.log"
  STAMP="$DATA/postinstall.version"
  LOCK="$DATA/postinstall.lock"
  VERSION="$(sed -n -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' "$PLUGIN_ROOT/.claude-plugin/plugin.json" 2>/dev/null | head -1)"
  if [[ -n "$VERSION" && "$(cat "$STAMP" 2>/dev/null)" != "$VERSION" ]]; then
    # A lock with no pid file yet is live for 10 minutes: the pid is written just
    # after mkdir, and a second session in that gap must not start a second install.
    if [[ -d "$LOCK" ]]; then
      holder="$(cat "$LOCK/pid" 2>/dev/null)"
      if [[ -n "$holder" ]]; then
        kill -0 "$holder" 2>/dev/null || rm -rf "$LOCK"
      elif [[ -n "$(find "$LOCK" -maxdepth 0 -mmin +10 2>/dev/null)" ]]; then
        rm -rf "$LOCK"
      fi
    fi
    if mkdir "$LOCK" 2>/dev/null; then
      last="$(tail -1 "$LOG" 2>/dev/null)"
      # Every descriptor is redirected: a child holding the hook's stdout keeps
      # Claude Code waiting on EOF for the whole install.
      nohup bash -c 'bash "$1"; rc=$?; echo "installer exit $rc"; [[ $rc -eq 0 ]] && printf "%s\n" "$4" >"$2"; rm -rf "$3"; exit $rc' \
        _ "$INSTALLER" "$STAMP" "$LOCK" "$VERSION" </dev/null >"$LOG" 2>&1 &
      echo $! >"$LOCK/pid" 2>/dev/null
      echo "## Installing the fno CLI"
      echo
      if [[ "$last" =~ ^installer\ exit\ ([1-9][0-9]*)$ ]]; then
        echo "The previous install attempt failed (installer exit ${BASH_REMATCH[1]}), so it runs again."
      fi
      echo "\`fno\` is not on your PATH yet. The footnote installer started in the background and logs to \`$LOG\`. Open a new session when the log ends with \`installer exit 0\`. Until then, verbs that shell out to \`fno\` fail."
      exit 0
    fi
    echo "## fno CLI install in progress"
    echo
    echo "\`fno\` is not on your PATH yet. Another session is running the footnote installer. It logs to \`$LOG\`. Open a new session when the log ends with \`installer exit 0\`."
    exit 0
  fi
  [[ -n "$VERSION" ]] && STAMPED_LOG="$LOG"
fi

cat <<'EOF'
## Install the `fno` front door

`fno` (the Rust mux front door) is not active on your PATH - you likely have `fno-py` (the Python CLI) only. Install the front door so bare `fno` works and bootstraps the rest: `cargo install fno` (needs a Rust toolchain), or `fno doctor update --rust` from a clone - see docs/getting-started.md for other methods. Until then, reach the CLI as `fno-py`.
EOF
if [[ -n "$STAMPED_LOG" ]]; then
  echo
  echo "The footnote installer already ran for this plugin version. Its log is \`$STAMPED_LOG\`. If it installed \`fno\`, put the tool bin directory (usually \`~/.local/bin\`) on your PATH."
fi
