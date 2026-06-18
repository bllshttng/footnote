#!/usr/bin/env bash
# SessionStart hook: inject `fno whoami` output as orientation context.
#
# Gives every fresh session an at-a-glance view of its operating stack
# (project + fleet + walker + session + provider) so the agent re-orients
# without grepping state files. After a compaction or in a long session,
# "I should run fno whoami to re-orient" is exactly the kind of
# detail that disappears; this hook fires it automatically.

set -uo pipefail

# Skip if fno is not installed - degrade silently rather than spam every
# session with errors in projects that don't have the plugin.
command -v fno >/dev/null 2>&1 || exit 0

# Skip if no .fno/ in the project - nothing to introspect.
[[ -d ".fno" ]] || exit 0

# Portable timeout: macOS lacks timeout(1) by default; coreutils installs it
# as gtimeout. When neither is present, fall back to bare invocation (fno
# itself returns quickly under normal conditions; the cap only matters for
# hung-binary edge cases).
_with_timeout() {
  local secs="$1"; shift
  if command -v timeout >/dev/null 2>&1; then
    timeout "$secs" "$@"
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout "$secs" "$@"
  else
    "$@"
  fi
}

# Run whoami; suppress stderr (warnings would otherwise leak into the
# injection blob). Cap at 2s wall-clock so a hung fno never blocks
# session start. `|| true` swallows rc=124 (timeout) cleanly.
OUTPUT=$(_with_timeout 2 fno whoami 2>/dev/null || true)
[[ -z "$OUTPUT" ]] && exit 0

# Emit as a fenced block so the formatting survives the injection.
cat <<EOF
## Agent operating stack

\`\`\`
$OUTPUT
\`\`\`

Re-run \`fno whoami\` anytime to refresh; \`fno status\` for the gate/events deep dive.
EOF
