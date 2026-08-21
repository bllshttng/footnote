#!/usr/bin/env bash
# SessionStart hook: remind the user to install the `fno` Rust front door when
# it is not active on PATH. The uv / `curl fno.sh | sh` / plugin channels land
# `fno-py` (the Python CLI console script), NOT the Rust `fno` mux front door
# (crates/fno) that owns `fno` on PATH and bootstraps `fno-py`. Without it, bare
# `fno` is command-not-found - and the fix is otherwise only visible if the user
# happens to run `fno doctor`. One advisory line; goes SILENT the moment the
# front door is active. Stdout becomes session context (same plain-text
# convention as setup-nudge-session-start.sh).

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

cat <<'EOF'
## Install the `fno` front door

`fno` (the Rust mux front door) is not active on your PATH - you likely have `fno-py` (the Python CLI) only. Install the front door so bare `fno` works and bootstraps the rest: `cargo install fno` (needs a Rust toolchain), or `fno doctor update --rust` from a clone - see docs/getting-started.md for other methods. Until then, reach the CLI as `fno-py`.
EOF
