#!/usr/bin/env bash
# Ensure ~/.fno/ exists and is accessible from sandboxed environments.
#
# Creates the global abilities directory and registers it with each detected
# AI CLI's sandbox/permissions so tools can read/write to it.
#
# Supports: Claude Code, Codex CLI, Gemini CLI
# Safe to run multiple times (idempotent).

set -uo pipefail

GLOBAL_DIR="$HOME/.fno"
mkdir -p "$GLOBAL_DIR/signals" "$GLOBAL_DIR/hooks"

echo "[ok] Global directory: $GLOBAL_DIR"

# ── Claude Code ──────────────────────────────────────────────────────────
# Add ~/.fno to additionalDirectories in settings.json
CLAUDE_SETTINGS="$HOME/.claude/settings.json"
if [ -f "$CLAUDE_SETTINGS" ] || command -v claude >/dev/null 2>&1; then
    if [ -f "$CLAUDE_SETTINGS" ]; then
        # Check if already registered
        if jq -e '.permissions.additionalDirectories // [] | index("~/.fno")' "$CLAUDE_SETTINGS" >/dev/null 2>&1; then
            echo "[ok] Claude Code: ~/.fno already in additionalDirectories"
        else
            # Add it
            TMP=$(mktemp)
            jq '.permissions.additionalDirectories = ((.permissions.additionalDirectories // []) + ["~/.fno"] | unique)' "$CLAUDE_SETTINGS" > "$TMP" && mv "$TMP" "$CLAUDE_SETTINGS"
            echo "[ok] Claude Code: added ~/.fno to additionalDirectories"
        fi
    else
        echo "[skip] Claude Code: no settings.json yet (will be created on first run)"
    fi
fi

# ── Codex CLI ────────────────────────────────────────────────────────────
# Codex uses .codex/ for config. Check if it has a similar directory allowlist.
CODEX_CONFIG="$HOME/.codex/config.json"
if [ -f "$CODEX_CONFIG" ] || command -v codex >/dev/null 2>&1; then
    if [ -f "$CODEX_CONFIG" ]; then
        if jq -e '.additionalDirectories // [] | index("~/.fno")' "$CODEX_CONFIG" >/dev/null 2>&1; then
            echo "[ok] Codex: ~/.fno already registered"
        else
            TMP=$(mktemp)
            jq '.additionalDirectories = ((.additionalDirectories // []) + ["~/.fno"] | unique)' "$CODEX_CONFIG" > "$TMP" && mv "$TMP" "$CODEX_CONFIG"
            echo "[ok] Codex: added ~/.fno to config"
        fi
    else
        echo "[skip] Codex: no config.json yet"
    fi
fi

# ── Gemini CLI ───────────────────────────────────────────────────────────
# Gemini uses .gemini/ for config.
GEMINI_SETTINGS="$HOME/.gemini/settings.json"
if [ -f "$GEMINI_SETTINGS" ] || command -v gemini >/dev/null 2>&1; then
    if [ -f "$GEMINI_SETTINGS" ]; then
        if jq -e '.sandbox.additionalDirectories // [] | index("~/.fno")' "$GEMINI_SETTINGS" >/dev/null 2>&1; then
            echo "[ok] Gemini: ~/.fno already registered"
        else
            TMP=$(mktemp)
            jq '.sandbox.additionalDirectories = ((.sandbox.additionalDirectories // []) + ["~/.fno"] | unique)' "$GEMINI_SETTINGS" > "$TMP" && mv "$TMP" "$GEMINI_SETTINGS"
            echo "[ok] Gemini: added ~/.fno to sandbox config"
        fi
    else
        echo "[skip] Gemini: no settings.json yet"
    fi
fi

echo ""
echo "Global abilities directory ready at $GLOBAL_DIR"
echo "Signals and cross-project state will be stored here."
