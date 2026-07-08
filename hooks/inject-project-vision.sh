#!/usr/bin/env bash
# SessionStart hook: inject project vision from settings.yaml into context
#
# Reads vision and goals from settings.yaml so the agent has semantic
# grounding from the start of every session. ~200 tokens, negligible cost.

set -uo pipefail

LOCAL_SETTINGS=".fno/config.toml"
if command -v fno >/dev/null 2>&1; then
    _PATHS_SH="$(fno paths shell-stub 2>/dev/null || true)"
    [[ -f "$_PATHS_SH" ]] && source "$_PATHS_SH" 2>/dev/null || true
    unset _PATHS_SH
fi
# GLOBAL_SETTINGS = per-user global; never alias CONFIG_FILE (active=local file). ab-5d6c3d47
GLOBAL_SETTINGS="${FNO_GLOBAL_SETTINGS_PATH:-$HOME/.fno/config.toml}"

# Find settings file (local override > global)
SETTINGS=""
if [[ -f "$LOCAL_SETTINGS" ]]; then
  SETTINGS="$LOCAL_SETTINGS"
elif [[ -f "$GLOBAL_SETTINGS" ]]; then
  SETTINGS="$GLOBAL_SETTINGS"
fi

if [[ -z "$SETTINGS" ]]; then
  exit 0
fi

# Read project.vision + project.goals from the flat config.toml via yq (TOML
# mode). Both degrade to empty when yq is absent or the key is unset, so the
# hook stays best-effort context injection. goals is an unmodeled array of
# {id, goal} tables; a hand-added list still renders one line per entry.
if command -v yq >/dev/null 2>&1; then
  VISION=$(yq -p toml -r '.project.vision // ""' "$SETTINGS" 2>/dev/null)
  GOALS=$(yq -p toml -r '.project.goals[]? | "- " + .id + ": " + .goal' "$SETTINGS" 2>/dev/null)
else
  VISION=""
  GOALS=""
fi

if [[ -z "$VISION" && -z "$GOALS" ]]; then
  # Settings found but nothing parsed — warn so user knows
  echo "inject-project-vision: settings.yaml found but no vision/goals parsed" >&2
  exit 0
fi

# Output as system context
{
  echo "## Project Vision"
  echo ""
  if [[ -n "$VISION" ]]; then
    echo "$VISION"
    echo ""
  fi
  if [[ -n "$GOALS" ]]; then
    echo "### Active Goals"
    echo ""
    echo "$GOALS"
  fi
}
