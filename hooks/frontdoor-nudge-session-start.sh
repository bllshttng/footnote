#!/usr/bin/env bash
# fno hook: SessionStart - frontdoor nudge session start
# SessionStart hook: when the `fno` Rust front door is not active on PATH, tell
# the agent what an install would do, name the one command that runs it, and
# start NOTHING. User ruling 2026-10-01: we always tell people before we
# install anything on their machine. The agent relays the notice, asks the
# user, and only a yes runs `bash <plugin root>/.claude-plugin/postinstall.sh`
# in the open - never detached, never from a hook. Goes SILENT the moment the
# front door is active - resolved beyond this session's PATH first
# (~/.local/bin, ~/.cargo/bin): a fresh background session's PATH lacks those,
# and reading a working install as missing once started the installer over a
# live tool env (2026-10-02 gap audit, blocker 1). A proven door off PATH prints
# a one-line hint naming the path instead of installing. Stdout becomes session
# context (same plain-text convention as setup-nudge-session-start.sh).

set -uo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

# The Rust front door answers a mux-only verb; the Python `fno-py` has no `mux`
# subcommand and fails "No such command". This is the same probe `fno doctor`'s
# `_probe_is_mux` uses. `fno mux ls --json` is read-only, returns `[]` with no
# server, and does not need the daemon, so it is fast. Bound it anyway so a
# wedged socket can never stall session start.
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/with-timeout.sh
source "$HOOK_DIR/../scripts/lib/with-timeout.sh" 2>/dev/null || exit 0

# Resolve the front door beyond this session's PATH: a fresh background
# session does not inherit ~/.cargo/bin, and `command -v fno` alone then reads
# a working install as missing, which started the installer and force-replaced
# a live tool env mid-study (2026-10-02 gap audit, blocker 1). The shared
# resolver in hooks/lib/fno-bin.sh serves this hook and the prompt-submit twin.
source "$HOOK_DIR/lib/fno-bin.sh" 2>/dev/null || true
FNO_BIN="$(fno_bin)"
if [[ -n "$FNO_BIN" ]]; then
  # The probe's EXIT CODE is the signal (2 = fno-py, 124 = hung socket that
  # still proves the Rust door), so it keeps its FIXED bound: the load-aware
  # budget's silence contract cannot express "the exit code is the answer",
  # and a runner under shard load measured a 1s fork miss that turned a real
  # rc 2 into a 124 and silenced a real reminder.
  with_timeout 3 "$FNO_BIN" mux ls --json >/dev/null 2>&1
  probe_rc=$?
  # 124 means our own bound fired. A wedged socket still PROVES the Rust front
  # door is present: `fno-py` has no `mux` verb and fails fast with a usage
  # error, it cannot hang here. Treating that as "not the front door" would nag a
  # user to install what they already have, every session, for as long as the
  # socket stays wedged. Now that the cap actually fires on every host rather
  # than only where Homebrew supplied timeout(1), that misread is reachable
  # everywhere, so it has to be distinguished from a real probe failure.
  if [[ $probe_rc -eq 0 || $probe_rc -eq 124 ]]; then
    if command -v fno >/dev/null 2>&1; then
      exit 0 # `fno` on PATH IS the Rust mux front door - nothing to remind
    fi
    # Installed but off this session's PATH. Reinstalling cannot help and can
    # only clobber the working tool env; name the path and stop.
    echo "## fno is installed, just not on this session's PATH"
    echo
    echo "\`fno\` lives at \`$FNO_BIN\`, which this session's PATH lacks. New sessions pick it up; this one can call it by that path or \`export PATH=\"\$(dirname \"$FNO_BIN\"):\$PATH\"\`."
    exit 0
  fi
fi

PLUGIN_ROOT="$(cd "$HOOK_DIR/.." && pwd)"
INSTALLER="$PLUGIN_ROOT/.claude-plugin/postinstall.sh"

if [[ ! -f "$INSTALLER" ]]; then
  echo "## fno is not installed, and the plugin tree carries no installer"
  echo
  echo "Expected the installer at \`$INSTALLER\` but it is missing; reinstall the footnote plugin. Until \`fno\` is installed, verbs that shell out to \`fno\` fail."
  exit 0
fi

# Say what the install would do to this machine, so the user can consent to
# THIS machine's shape: with uv present the installer skips the astral.sh step
# entirely and touches nothing but the fno tool.
if command -v uv >/dev/null 2>&1; then
  UV_STEP="uv is already installed (\`$(command -v uv)\`), so that step is skipped and the install touches nothing but the fno tool."
else
  UV_STEP="It first runs the uv installer from https://astral.sh/uv/install.sh, which installs uv and edits your shell profile to put it on PATH."
fi

cat <<EOF
## Install the fno CLI? Ask the user first

\`fno\` (the footnote CLI) is not installed on this machine. The plugin ships an installer, but nothing runs it without the user's explicit yes.

What the installer does: $UV_STEP Then it runs \`uv tool install fno\`, which downloads \`fno\` from PyPI into the uv tool environment and puts the \`fno\` commands in the tool bin directory (usually \`~/.local/bin\`).
EOF

# The probe already resolved a Python-only `fno-py` (rc 2): say so, so the
# notice never reads as "nothing of ours is here".
if [[ -n "$FNO_BIN" ]]; then
  echo
  echo "A Python \`fno-py\` already lives beside \`$FNO_BIN\`; verbs that only need the Python CLI work as \`fno-py\` until the full install lands."
fi

cat <<EOF

The one command that runs the install: \`bash $INSTALLER\`

You (the agent) must tell the user what it does, ask, and run it only on a yes - never unasked, never detached in the background. It takes a few minutes. Its output ends with \`installer exit 0\` on success; any other code on that final line means it failed.
EOF
