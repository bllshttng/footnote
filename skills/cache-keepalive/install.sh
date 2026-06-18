#!/usr/bin/env bash
# Install cache-keepalive as a standalone skill.
#
# Usage:
#   bash install.sh
#
# What it does:
#   1. Copies the skill to ~/.claude/skills/cache-keepalive/
#   2. Copies the SessionStart hook to ~/.claude/skills/cache-keepalive/
#
# Hook registration is handled by the plugin system (.claude-plugin/).
# To enable auto-activation, add to your project's .claude/settings.local.json:
#   { "cacheKeepalive": true }

set -euo pipefail

SKILL_DIR="$HOME/.claude/skills/cache-keepalive"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

echo "Installing cache-keepalive..."

# 1. Copy skill
mkdir -p "$SKILL_DIR"
cp "$SCRIPT_DIR/SKILL.md" "$SKILL_DIR/"

# 2. Copy hook script
cp "$REPO_ROOT/hooks/cache-keepalive-inject.sh" "$SKILL_DIR/"
chmod +x "$SKILL_DIR/cache-keepalive-inject.sh"

echo ""
echo "Installed to $SKILL_DIR"
echo ""
echo "Manual use: /cache-keepalive"
echo ""
echo "Auto-activation: add to .claude/settings.local.json:"
echo '  { "cacheKeepalive": true }'
echo ""
echo "Then add a SessionStart hook pointing to:"
echo "  $SKILL_DIR/cache-keepalive-inject.sh"
