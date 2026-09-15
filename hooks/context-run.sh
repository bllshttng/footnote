#!/usr/bin/env bash
# One runner for every fno context producer. The groups live in
# hooks/context-hooks.json; fno-agents context-run runs one group and writes
# one context_snapshot.
set -u
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HOOK_DIR/.." && pwd)"
# shellcheck source=lib/shellcheck-not-a-file
source "$HOOK_DIR/lib/agents-bin.sh"
BIN="$(fno_agents_bin "$ROOT")"
if [[ -z "$BIN" ]]; then
    echo "fno: context-run unavailable (fno-agents not found); run fno doctor --fix" >&2
    exit 0
fi
exec "$BIN" context-run --group "${1:-}" --plugin-root "$ROOT"
