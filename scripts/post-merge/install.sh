#!/usr/bin/env bash
# install.sh - render the per-repo post-merge watcher LaunchAgent for THIS repo
# and write it to ~/Library/LaunchAgents, then PRINT the rendered plist and the
# exact `launchctl load` command for the human to run.
#
# HUMAN GATE (ab-4e9fb05a): this installer NEVER loads the agent. It is
# system-touching (a LaunchAgent runs `claude --print` headlessly on an
# interval), so a human must review the rendered plist and load it themselves.
# If you are an automated agent, STOP here - do not run `launchctl load`.
#
# Config (env, optional):
#   POST_MERGE_INTERVAL - poll cadence in seconds (default 600; 5-15 min suggested).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMPLATE="${SCRIPT_DIR}/com.fno.postmerge.plist.template"
WATCH_SCRIPT="${SCRIPT_DIR}/watch.sh"

[[ -f "$TEMPLATE" ]]     || { echo "install: template not found at $TEMPLATE" >&2; exit 1; }
[[ -f "$WATCH_SCRIPT" ]] || { echo "install: watch.sh not found at $WATCH_SCRIPT" >&2; exit 1; }

# Script-relative fallback (not pwd): install.sh lives in scripts/post-merge/.
REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || (cd "$SCRIPT_DIR/../.." && pwd))"
REPO_NAME="$(basename "$REPO_ROOT")"
INTERVAL="${POST_MERGE_INTERVAL:-600}"
[[ "$INTERVAL" =~ ^[0-9]+$ ]] || { echo "install: POST_MERGE_INTERVAL must be an integer (got '$INTERVAL')" >&2; exit 1; }

# Unique label per CHECKOUT: two clones sharing a basename (fork + upstream, or
# two `abilities` clones) would otherwise render the same label + plist path and
# clobber each other's agent (Codex P2, PR #390). Append a short hash of the
# absolute repo root. Shared with uninstall.sh's resolution.
ROOT_HASH="$(printf '%s' "$REPO_ROOT" | shasum 2>/dev/null | cut -d' ' -f1 | cut -c1-8)"
[[ -n "$ROOT_HASH" ]] || ROOT_HASH="$(printf '%s' "$REPO_ROOT" | cksum | tr -cd '0-9' | cut -c1-8)"
LABEL="com.fno.postmerge.${REPO_NAME}-${ROOT_HASH}"
PLIST_DIR="${HOME}/Library/LaunchAgents"
PLIST_PATH="${PLIST_DIR}/${LABEL}.plist"
LOG_OUT="${REPO_ROOT}/.fno/post-merge-watch.out.log"
LOG_ERR="${REPO_ROOT}/.fno/post-merge-watch.err.log"
# Capture the install-time PATH so the LaunchAgent (which launchd gives a minimal
# PATH) can resolve gh/jq/claude where they actually live (Codex P1, PR #390).
RENDER_PATH="$PATH"
# Model for the per-merge ritual fire; defaults to Haiku (cheap). Override with
# POST_MERGE_MODEL at install time (e.g. POST_MERGE_MODEL=sonnet bash install.sh).
MODEL_VAL="${POST_MERGE_MODEL:-claude-haiku-4-5}"

# XML-escape substituted values: a checkout path with & or < (legal on macOS)
# would otherwise be written raw into <string> values and produce a plist that
# launchctl/plutil reject (Codex P2, PR #390). & must be escaped first.
xml_escape() {
  local s="$1"
  s="${s//&/&amp;}"; s="${s//</&lt;}"; s="${s//>/&gt;}"
  s="${s//\"/&quot;}"; s="${s//\'/&apos;}"
  printf '%s' "$s"
}

# Render via bash string replacement (robust against & | / in paths, unlike sed).
content="$(cat "$TEMPLATE")"
content="${content//\{\{LABEL\}\}/$(xml_escape "$LABEL")}"
content="${content//\{\{WATCH_SCRIPT\}\}/$(xml_escape "$WATCH_SCRIPT")}"
content="${content//\{\{REPO_ROOT\}\}/$(xml_escape "$REPO_ROOT")}"
content="${content//\{\{INTERVAL\}\}/$INTERVAL}"
content="${content//\{\{LOG_OUT\}\}/$(xml_escape "$LOG_OUT")}"
content="${content//\{\{LOG_ERR\}\}/$(xml_escape "$LOG_ERR")}"
content="${content//\{\{PATH\}\}/$(xml_escape "$RENDER_PATH")}"
content="${content//\{\{MODEL\}\}/$(xml_escape "$MODEL_VAL")}"

mkdir -p "$PLIST_DIR"
# Ensure the repo's .fno/ exists so the plist's StandardOut/ErrPath (and
# the watcher's watermark) resolve at load time on a fresh clone.
mkdir -p "${REPO_ROOT}/.fno"
printf '%s\n' "$content" > "$PLIST_PATH"

cat <<EOF

post-merge watcher: rendered LaunchAgent written to
  ${PLIST_PATH}
  (label: ${LABEL}, repo: ${REPO_ROOT}, interval: ${INTERVAL}s)

----------------------------- rendered plist -----------------------------
${content}
--------------------------------------------------------------------------

This installer did NOT load the agent (human gate). Review the plist above,
then load it yourself:

  launchctl load ${PLIST_PATH}

Verify it is registered:

  launchctl list | grep ${LABEL}

To remove it later:

  bash ${SCRIPT_DIR}/uninstall.sh
EOF
