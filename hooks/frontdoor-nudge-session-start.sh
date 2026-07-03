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
# server, and does not need the daemon, so it is fast. Bound it anyway (via
# timeout when available) so a wedged socket can never stall session start.
_timeout=""
if command -v timeout >/dev/null 2>&1; then
  _timeout="timeout 3"
elif command -v gtimeout >/dev/null 2>&1; then
  _timeout="gtimeout 3"
fi

if command -v fno >/dev/null 2>&1 && $_timeout fno mux ls --json >/dev/null 2>&1; then
  exit 0 # `fno` on PATH IS the Rust mux front door - nothing to remind
fi

cat <<'EOF'
## Install the `fno` front door

`fno` (the Rust mux front door) is not active on your PATH - you likely have `fno-py` (the Python CLI) only. Install the front door so bare `fno` works and bootstraps the rest: `cargo install fno` (needs a Rust toolchain), or `fno update --rust` from a clone - see docs/getting-started.md for other methods. Until then, reach the CLI as `fno-py`.
EOF
