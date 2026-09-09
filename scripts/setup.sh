#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "$0")/.." && pwd)"
# shellcheck source=./lib/codex_utils.sh
source "$ROOT_DIR/scripts/lib/codex_utils.sh"
PROVIDER=""
CODEX_SKILLS_ROOT=""
SKILLS_SOURCE="auto"
PYTHON="${FNO_PYTHON:-python3}"
SKILL_DISCOVERY_PY="$ROOT_DIR/scripts/lib/skill_discovery.py"

usage() {
  cat <<USAGE
Usage: ./scripts/setup.sh [options]

Options:
  --provider <name>        Provider-specific setup to run (currently: codex)
  --skills-root <path>     Codex skills root to populate (default: .agents/skills)
  --skills-source <mode>   installed | development | auto (default: auto).
                           installed: the installed Footnote plugin is the one
                           discovery source; source aliases are not advertised.
                           development: source skills/ are aliased for local dev.
  --skip-package-setup     Compatibility no-op for bootstrap jobs
  -h, --help               Show this help
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
    --skills-source)
      if [[ -z "${2:-}" ]]; then
        echo "Error: --skills-source requires a value." >&2
        usage
        exit 1
      fi
      case "$2" in
        auto|installed|development) SKILLS_SOURCE="$2" ;;
        *)
          echo "Error: --skills-source must be auto, installed or development." >&2
          exit 1
          ;;
      esac
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
  # One resolver for setup and doctor (scripts/doctor.sh), so both can never
  # disagree about which root is checked. The recorded root survives re-runs.
  local skills_root
  skills_root="$(codex_resolve_skills_root "$ROOT_DIR" "$CODEX_SKILLS_ROOT" "$ROOT_DIR/.fno/codex-skills-root")"
  mkdir -p "$skills_root" "$ROOT_DIR/.fno"

  # Remove unrelated foreign links from THIS project's discovery only.
  # Real directories and shared sources are never touched; every removal
  # prints its restore line.
  echo "Codex: curating $skills_root"
  "$PYTHON" "$SKILL_DISCOVERY_PY" curate --repo "$ROOT_DIR" --root "$skills_root" --apply

  local plugin_line plugin_path plugin_version probe_args=()
  if [[ -n "${CODEX_PLUGIN_CACHE:-}" ]]; then
    probe_args=(--cache "$CODEX_PLUGIN_CACHE")
  fi
  plugin_line="$("$PYTHON" "$SKILL_DISCOVERY_PY" probe --repo "$ROOT_DIR" "${probe_args[@]+${probe_args[@]}}")"
  plugin_path="$(sed -n 's/^plugin=//p' <<< "$plugin_line")"
  plugin_version="$(sed -n 's/^version=//p' <<< "$plugin_line")"

  local mode="$SKILLS_SOURCE"
  if [[ "$mode" == "auto" ]]; then
    if [[ -n "$plugin_path" ]]; then
      mode="installed"
    else
      mode="development"
    fi
  fi

  if [[ "$mode" == "installed" ]]; then
    local removed
    removed=$(find "$skills_root" -maxdepth 1 -type l \( -name 'fno--*' -o -name 'plugin--fno--*' \) | wc -l | tr -d ' ')
    find "$skills_root" -maxdepth 1 -type l \( -name 'fno--*' -o -name 'plugin--fno--*' \) -exec /bin/rm -f {} +
    echo "Codex: installed plugin ${plugin_version:-?} at ${plugin_path:-?} is the single skill source"
    echo "Codex: removed $removed source aliases (restore with --skills-source development)"
    # Names the plugin does not ship are reported, never silently missing.
    "$PYTHON" "$SKILL_DISCOVERY_PY" inventory --repo "$ROOT_DIR" --root "$skills_root" "${probe_args[@]+${probe_args[@]}}" \
      | awk '$2 == "absent-from-plugin" { print "Codex: [gap] " $1 " is not in the installed plugin; reinstall to restore" }'
  else
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
    local revision="unknown"
    revision="$(git -C "$ROOT_DIR" rev-parse --short HEAD 2>/dev/null || echo unknown)"
    echo "Codex: development source $ROOT_DIR/skills @ $revision"
    echo "Codex: linked $count skills into $skills_root"
    if [[ -n "$plugin_path" ]]; then
      echo "Codex: [note] installed plugin $plugin_version also exposes skills; both sources are live by explicit choice"
    fi
  fi
  printf '%s\n' "$skills_root" > "$ROOT_DIR/.fno/codex-skills-root"
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
