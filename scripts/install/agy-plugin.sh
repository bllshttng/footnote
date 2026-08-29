#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MANIFEST="$ROOT_DIR/plugin.json"
PLUGIN_HOME="${AGY_PLUGIN_HOME:-${HOME:?}/.gemini/antigravity-cli/plugins}"
TIMEOUT_S="${AGY_PLUGIN_INSTALL_TIMEOUT_S:-60}"
STALE_MB="${AGY_PLUGIN_STALE_MB:-100}"

say() { printf 'agy-plugin: %s\n' "$*" >&2; }
die() { say "ERROR: $*"; exit 1; }

BUILD_ONLY=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --build-only)
      [[ -n "${2:-}" ]] || die "--build-only requires a target directory"
      BUILD_ONLY="$2"
      shift 2
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

command -v jq >/dev/null 2>&1 || die "jq is required to validate plugin.json."
[[ -f "$MANIFEST" ]] || die "missing plugin manifest: $MANIFEST"

PLUGIN_NAME="$(jq -er '.name' "$MANIFEST")" || die "plugin.json has no valid name"
[[ "$PLUGIN_NAME" =~ ^[a-zA-Z0-9_-]+$ ]] || die "invalid plugin name: $PLUGIN_NAME"
[[ "$TIMEOUT_S" =~ ^[0-9]+$ ]] || die "AGY_PLUGIN_INSTALL_TIMEOUT_S must be an integer"
[[ "$STALE_MB" =~ ^[0-9]+$ ]] || die "AGY_PLUGIN_STALE_MB must be an integer"

# agy 1.1.16 stages to ~/.gemini/config/plugins/<name>/ (measured); the CLI
# reference documents ~/.gemini/antigravity-cli/plugins instead. Both are real
# candidate paths; config/ goes first because it is where agy actually puts it.
STAGE_CANDIDATES=(
  "${HOME:?}/.gemini/config/plugins/$PLUGIN_NAME"
  "$PLUGIN_HOME/$PLUGIN_NAME"
)

# Copy an explicit allowlist, never $ROOT_DIR: handing agy the whole checkout
# dereferenced internal/ into a 9.1 GB half-copy. cp -R per entry so an
# unlisted path cannot ride along.
build_payload() {
  local dest_parent="$1"
  local payload="$dest_parent/$PLUGIN_NAME"
  local entry
  for entry in "$MANIFEST" "$ROOT_DIR/skills" "$ROOT_DIR/agents"; do
    [[ -e "$entry" ]] || die "payload allowlist entry missing: $entry"
  done
  mkdir -p "$payload"
  cp "$MANIFEST" "$payload/plugin.json"
  cp -R "$ROOT_DIR/skills" "$payload/skills"
  cp -R "$ROOT_DIR/agents" "$payload/agents"
  printf '%s\n' "$payload"
}

if [[ -n "$BUILD_ONLY" ]]; then
  payload="$(build_payload "$BUILD_ONLY")"
  say "payload built: $payload"
  exit 0
fi

command -v agy >/dev/null 2>&1 || die "agy is not on PATH. Install Antigravity CLI first."

# A fresh install merges into an existing stage rather than replacing it. The
# 9.1 GB partial this script once left behind is not ours to delete for the
# operator; refuse and name the command instead. An unmeasurable stage refuses
# too (fail closed): we cannot prove it is safe to merge into.
for stage in "${STAGE_CANDIDATES[@]}"; do
  [[ -d "$stage" ]] || continue
  stage_mb="$(du -sm "$stage" 2>/dev/null | cut -f1 || true)"
  if [[ -z "$stage_mb" ]]; then
    die "cannot measure stage at $stage (du failed; unreadable?); a fresh install would merge into it. Remove it first: rm -rf \"$stage\""
  fi
  if (( stage_mb > STALE_MB )); then
    die "stale stage at $stage holds ${stage_mb} MB (limit ${STALE_MB} MB); a fresh install would merge into it. Remove it first: rm -rf \"$stage\""
  fi
done

PAYLOAD_PARENT="$(mktemp -d "${TMPDIR:-/tmp}/fno-agy-plugin.XXXXXX")"
trap 'rm -rf "$PAYLOAD_PARENT"' EXIT
build_payload "$PAYLOAD_PARENT" >/dev/null

say "installing $PLUGIN_NAME from $PAYLOAD_PARENT/$PLUGIN_NAME"
agy plugin install "$PAYLOAD_PARENT/$PLUGIN_NAME" &
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

# A staged plugin.json is a snapshot of a file copy, not proof agy loaded the
# plugin - the 9.1 GB case had one and agy still listed nothing. The marker is
# agy naming the plugin itself, matched on the name's own character class so a
# superstring like footnote-dev cannot answer for footnote. The name is
# validated to ^[a-zA-Z0-9_-]+$ above, so interpolating it into the regex is
# metachar-safe.
list_out="$(agy plugin list 2>&1 || true)"
if grep -qE "(^|[^a-zA-Z0-9_-])${PLUGIN_NAME}([^a-zA-Z0-9_-]|$)" <<<"$list_out"; then
  say "installed and discovered: $PLUGIN_NAME"
  exit 0
fi

say "agy plugin list does not name $PLUGIN_NAME. Staged paths checked:"
for candidate in "${STAGE_CANDIDATES[@]}"; do
  if [[ -f "$candidate/plugin.json" ]] && jq -e --arg name "$PLUGIN_NAME" '.name == $name' "$candidate/plugin.json" >/dev/null 2>&1; then
    say "  staged but undiscovered: $candidate"
  else
    say "  not staged: $candidate"
  fi
done
exit 1
