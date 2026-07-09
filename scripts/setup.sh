#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "$0")/.." && pwd)"
PROVIDER=""
CODEX_SKILLS_ROOT=""

usage() {
  cat <<USAGE
Usage: ./scripts/setup.sh [options]

Options:
  --provider <name>      Provider-specific setup to run (currently: codex)
  --skills-root <path>   Codex skills root to populate (default: .agents/skills)
  --skip-package-setup   Compatibility no-op for bootstrap jobs
  -h, --help             Show this help
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --provider)
      if [[ -z "${2:-}" ]]; then
        echo "Error: --provider requires a value." >&2
        usage
        exit 1
      fi
      PROVIDER="$2"
      shift 2
      ;;
    --skills-root)
      if [[ -z "${2:-}" ]]; then
        echo "Error: --skills-root requires a path." >&2
        usage
        exit 1
      fi
      CODEX_SKILLS_ROOT="$2"
      shift 2
      ;;
    --skip-package-setup)
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage
      exit 1
      ;;
  esac
done

setup_codex() {
  local skills_root="${CODEX_SKILLS_ROOT:-$ROOT_DIR/.agents/skills}"
  mkdir -p "$skills_root" "$ROOT_DIR/.fno"

  # `plugin--fno--<skill>` matches doctor.sh's plugin-link convention and keeps
  # local dev links distinct from Codex-native agent/skill material.
  find "$skills_root" -maxdepth 1 -type l \( -name 'fno--*' -o -name 'plugin--fno--*' \) -exec /bin/rm -f {} +
  local count=0
  local skill
  for skill in "$ROOT_DIR"/skills/*; do
    [[ -f "$skill/SKILL.md" ]] || continue
    ln -sfn "$skill" "$skills_root/plugin--fno--$(basename -- "$skill")"
    count=$((count + 1))
  done
  printf '%s\n' "$skills_root" > "$ROOT_DIR/.fno/codex-skills-root"
  echo "Codex: linked $count skills into $skills_root"
}

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

case "$PROVIDER" in
  "")
    ;;
  codex)
    setup_codex
    ;;
  *)
    echo "Unknown provider: $PROVIDER" >&2
    echo "Supported providers: codex" >&2
    exit 1
    ;;
esac

echo "Abilities setup complete"
