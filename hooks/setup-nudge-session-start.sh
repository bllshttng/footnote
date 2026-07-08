#!/usr/bin/env bash
# SessionStart hook: nudge a brand-new user toward setup when no fno config
# exists yet. Install lands the CLI but never prompts, so the setup wizard is
# otherwise undiscoverable. One advisory line; goes silent the moment any
# settings file exists. Stdout becomes session context (same plain-text
# convention as inject-project-vision.sh).

set -uo pipefail

LOCAL_SETTINGS=".fno/config.toml"
# GLOBAL_SETTINGS = per-user global; never alias the active/local file.
GLOBAL_SETTINGS="${FNO_GLOBAL_SETTINGS_PATH:-$HOME/.fno/config.toml}"

# Silent once configured anywhere (local override > global).
if [[ -f "$LOCAL_SETTINGS" || -f "$GLOBAL_SETTINGS" ]]; then
  exit 0
fi

cat <<'EOF'
## First-run setup

No fno config found yet. Run `fno setup wizard` (terminal) or `/fno:setup` (in a Claude Code session) to configure. Optional - defaults work, so `/fno:target "..."` runs without it.
EOF
