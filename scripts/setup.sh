#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "$0")/.." && pwd)"

bash "$ROOT_DIR/scripts/preflight.sh"
bash "$ROOT_DIR/scripts/ensure-global-dir.sh"
mkdir -p "$ROOT_DIR/.fno/checkpoints"

# Scaffold settings.yaml with project vision placeholders if it doesn't exist
SETTINGS_FILE="$ROOT_DIR/.fno/config.toml"
if [[ ! -f "$SETTINGS_FILE" ]]; then
  cat > "$SETTINGS_FILE" << 'EOF'
# Project Settings (flat config.toml)
# Configure via /setup wizard or edit directly.

[project]
# What does this project do? Who is it for?
vision = ""
# SMART goals or OKRs for this project
goals = []
# Budget, team size, technical constraints
constraints = []

# Do-Target configuration
[target.defaults]
max_iterations = 40

# External code review
[review]
provider = "gemini"
# provider: coderabbit | claude | codex
EOF
  echo "Created $SETTINGS_FILE — edit directly or run /setup"
fi

echo "Abilities setup complete"
