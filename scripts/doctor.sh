#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "$0")/.." && pwd)"
# shellcheck source=./lib/codex_utils.sh
source "$ROOT_DIR/scripts/lib/codex_utils.sh"
FAIL=0
SKILLS_ROOT="${CODEX_SKILLS_ROOT:-}"
SKILLS_ROOT_RECORD="$ROOT_DIR/.fno/codex-skills-root"

usage() {
  cat <<USAGE
Usage: ./scripts/doctor.sh [options]

Options:
  --skills-root <path>   Override Codex skills root checked by doctor
  -h, --help             Show this help
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --skills-root)
      if [[ -z "${2:-}" ]]; then
        echo "Error: --skills-root requires a path." >&2
        usage
        exit 1
      fi
      SKILLS_ROOT="$2"
      shift 2
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

display_path() {
  local path="$1"
  case "$path" in
    "$ROOT_DIR"/*)
      printf '%s\n' "${path#$ROOT_DIR/}"
      ;;
    *)
      printf '%s\n' "$path"
      ;;
  esac
}

check_required() {
  local dep="$1"
  if command -v "$dep" >/dev/null 2>&1; then
    printf '  [ok] %s\n' "$dep"
  else
    printf '  [missing] %s\n' "$dep"
    FAIL=1
  fi
}

check_optional() {
  local dep="$1"
  if command -v "$dep" >/dev/null 2>&1; then
    printf '  [ok] %s\n' "$dep"
  else
    printf '  [optional-missing] %s\n' "$dep"
  fi
}

echo "== Core dependencies =="
check_required bash
check_required git
check_required gh
check_required jq

echo
echo "== Optional dependencies =="
check_optional python3
check_optional node
check_optional npm
check_optional pnpm
check_optional bun
check_optional claude
check_optional codex

echo
echo "== Codex skill links =="
if command -v codex >/dev/null 2>&1; then
  SKILLS_ROOT_RESOLVED="$(codex_resolve_skills_root "$ROOT_DIR" "$SKILLS_ROOT" "$SKILLS_ROOT_RECORD")"
  echo "  [info] root: $(display_path "$SKILLS_ROOT_RESOLVED")"
  if [[ ! -d "$SKILLS_ROOT_RESOLVED" ]]; then
    echo "  [missing] skills root (run ./scripts/setup.sh --provider codex${SKILLS_ROOT:+ --skills-root \"$SKILLS_ROOT\"})"
    FAIL=1
  elif ! codex_dir_is_writable_or_creatable "$SKILLS_ROOT_RESOLVED" || ! codex_dir_supports_mutation "$SKILLS_ROOT_RESOLVED"; then
    echo "  [blocked] skills root is not writable by this process"
    echo "  [hint] rerun setup/doctor with --skills-root \"\$HOME/.agents/skills\" or another writable Codex-scanned directory"
    FAIL=1
  else
    BROKEN=0
    COUNT=0
    while IFS= read -r link; do
      COUNT=$((COUNT + 1))
      if [[ ! -e "$link" ]]; then
        echo "  [broken] $(display_path "$link")"
        BROKEN=$((BROKEN + 1))
        FAIL=1
      fi
    done < <(find "$SKILLS_ROOT_RESOLVED" -maxdepth 1 -type l \( -name 'codex--*' -o -name 'plugin--*' \) | sort)

    echo "  [info] symlinks: $COUNT"
    if [[ "$BROKEN" -eq 0 ]]; then
      echo "  [ok] no broken symlinks"
    fi
  fi
else
  echo "  [skipped] codex not installed"
fi

echo
echo "== Abilities soft-hook scripts =="
for f in scripts/hooks/session-start.sh scripts/hooks/pre-compact.sh scripts/hooks/pre-tool-use.sh scripts/hooks/session-end.sh; do
  if [[ -x "$ROOT_DIR/$f" ]]; then
    echo "  [ok] $f"
  else
    echo "  [missing] $f"
    FAIL=1
  fi
done

if [[ "$FAIL" -ne 0 ]]; then
  echo
  echo "Doctor found issues."
  exit 1
fi

echo
echo "Doctor checks passed."
