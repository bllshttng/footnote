#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MANIFEST="$ROOT_DIR/plugin.json"
PLUGIN_HOME="${AGY_PLUGIN_HOME:-${HOME:?}/.gemini/antigravity-cli/plugins}"
TIMEOUT_S="${AGY_PLUGIN_INSTALL_TIMEOUT_S:-60}"

say() { printf 'agy-plugin: %s\n' "$*" >&2; }
die() { say "ERROR: $*"; exit 1; }

command -v agy >/dev/null 2>&1 || die "agy is not on PATH. Install Antigravity CLI first."
command -v jq >/dev/null 2>&1 || die "jq is required to validate plugin.json."
[[ -f "$MANIFEST" ]] || die "missing plugin manifest: $MANIFEST"

PLUGIN_NAME="$(jq -er '.name' "$MANIFEST")" || die "plugin.json has no valid name"
[[ "$PLUGIN_NAME" =~ ^[a-zA-Z0-9_-]+$ ]] || die "invalid plugin name: $PLUGIN_NAME"
[[ "$TIMEOUT_S" =~ ^[0-9]+$ ]] || die "AGY_PLUGIN_INSTALL_TIMEOUT_S must be an integer"

say "installing $PLUGIN_NAME from $ROOT_DIR"
agy plugin install "$ROOT_DIR" &
install_pid=$!
elapsed=0
while kill -0 "$install_pid" 2>/dev/null; do
  if (( elapsed >= TIMEOUT_S )); then
    kill "$install_pid" 2>/dev/null || true
    wait "$install_pid" 2>/dev/null || true
    die "agy plugin install exceeded ${TIMEOUT_S}s"
  fi
  sleep 1
  elapsed=$((elapsed + 1))
done
wait "$install_pid" || die "agy plugin install failed"

for candidate in \
  "$PLUGIN_HOME/$PLUGIN_NAME/plugin.json" \
  "${HOME:?}/.gemini/config/plugins/$PLUGIN_NAME/plugin.json"; do
  if jq -e --arg name "$PLUGIN_NAME" '.name == $name' "$candidate" >/dev/null 2>&1; then
    say "installed and verified: $candidate"
    exit 0
  fi
done

die "agy reported success, but no staged plugin.json was found"
