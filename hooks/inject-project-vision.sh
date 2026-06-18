#!/usr/bin/env bash
# SessionStart hook: inject project vision from settings.yaml into context
#
# Reads vision and goals from settings.yaml so the agent has semantic
# grounding from the start of every session. ~200 tokens, negligible cost.

set -uo pipefail

LOCAL_SETTINGS=".fno/settings.yaml"
if command -v fno >/dev/null 2>&1; then
    _PATHS_SH="$(fno paths shell-stub 2>/dev/null || true)"
    [[ -f "$_PATHS_SH" ]] && source "$_PATHS_SH" 2>/dev/null || true
    unset _PATHS_SH
fi
# GLOBAL_SETTINGS = per-user global; never alias CONFIG_FILE (active=local file). ab-5d6c3d47
GLOBAL_SETTINGS="${FNO_GLOBAL_SETTINGS_PATH:-$HOME/.fno/settings.yaml}"

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

# Extract vision (handles both inline and block scalar)
# Inline: vision: "text" or vision: text
# Block:  vision: >
#           multi-line text
VISION=$(awk '
  /^  vision:/ {
    # Try inline value first (vision: "text" or vision: text)
    inline = $0
    sub(/^  vision:[[:space:]]*>?[[:space:]]*/, "", inline)
    gsub(/^"/, "", inline); gsub(/"[[:space:]]*$/, "", inline)
    if (length(inline) > 0 && inline != ">") { print inline; exit }
    found=1; next
  }
  found && /^  #/ { next }
  found && /^  [a-z]/ { exit }
  found && /^[a-z]/ { exit }
  found { gsub(/^    /, ""); print }
' "$SETTINGS")

# Extract goal lines (just the id + goal text, not full KRs)
# Strips quotes from both id and goal values
GOALS=$(awk '
  /^    - id:/ {
    id = $NF
    gsub(/"/, "", id)
  }
  /^      goal:/ {
    line = $0
    gsub(/^      goal:[[:space:]]*"?/, "", line)
    gsub(/"[[:space:]]*$/, "", line)
    print "- " id ": " line
  }
' "$SETTINGS")

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
