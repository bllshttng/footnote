#!/usr/bin/env bash
# install-corrections-git-hook.sh - register the ~/.claude/ post-commit hook.
#
# Idempotent. Safe to re-run. Symlinks ~/.claude/.git/hooks/post-commit ->
# the canonical hooks/corrections-git-postcommit.sh in this repo.
#
# If ~/.claude is not a git repo, prints instructions and offers to git init
# (declined by default - the writer becomes a no-op rather than mutating user state).
#
# Flags:
#   --yes        Skip the interactive git-init prompt (auto-decline)
#   --git-init   Skip the prompt and proceed with git init

set -euo pipefail

CLAUDE_DIR="${CLAUDE_DIR_OVERRIDE:-$HOME/.claude}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HOOK_SOURCE="$REPO_ROOT/hooks/corrections-git-postcommit.sh"
HOOK_TARGET="$CLAUDE_DIR/.git/hooks/post-commit"

AUTO_DECLINE=0
AUTO_INIT=0
for arg in "$@"; do
  case "$arg" in
    --yes) AUTO_DECLINE=1 ;;
    --git-init) AUTO_INIT=1 ;;
    -h|--help)
      sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
  esac
done

if [[ ! -d "$CLAUDE_DIR" ]]; then
  echo "install-corrections-git-hook: $CLAUDE_DIR does not exist" >&2
  echo "install-corrections-git-hook: install Claude Code first" >&2
  exit 1
fi

if [[ ! -f "$HOOK_SOURCE" ]]; then
  echo "install-corrections-git-hook: source hook not found at $HOOK_SOURCE" >&2
  exit 1
fi

# Bootstrap corrections.log if missing.
bash "$SCRIPT_DIR/corrections-log-init.sh"

if [[ ! -d "$CLAUDE_DIR/.git" ]]; then
  echo "install-corrections-git-hook: $CLAUDE_DIR is not a git repository" >&2
  echo "The post-commit writer needs ~/.claude to be a git repo to fire." >&2
  if [[ "$AUTO_INIT" == "1" ]]; then
    REPLY="y"
  elif [[ "$AUTO_DECLINE" == "1" ]]; then
    REPLY="n"
  else
    read -r -p "Initialize ~/.claude as a git repo now? [y/N] " REPLY || REPLY="n"
  fi
  case "$REPLY" in
    y|Y|yes|YES)
      ( cd "$CLAUDE_DIR" && git init -q )
      echo "install-corrections-git-hook: initialized $CLAUDE_DIR as a git repo" >&2
      ;;
    *)
      echo "install-corrections-git-hook: skipped git init; post-commit writer will be a no-op" >&2
      exit 0
      ;;
  esac
fi

mkdir -p "$CLAUDE_DIR/.git/hooks"

# Idempotent symlink: if target points to source, do nothing.
if [[ -L "$HOOK_TARGET" ]]; then
  CURRENT=$(readlink "$HOOK_TARGET")
  if [[ "$CURRENT" == "$HOOK_SOURCE" ]]; then
    echo "install-corrections-git-hook: already installed (no change)" >&2
    exit 0
  fi
  rm "$HOOK_TARGET"
elif [[ -e "$HOOK_TARGET" ]]; then
  # Existing real file. Back up before replacing so we never lose user state.
  BACKUP="${HOOK_TARGET}.backup.$(date -u +%Y%m%dT%H%M%SZ)"
  mv "$HOOK_TARGET" "$BACKUP"
  echo "install-corrections-git-hook: backed up existing hook to $BACKUP" >&2
fi

ln -s "$HOOK_SOURCE" "$HOOK_TARGET"
# Note: we do NOT chmod the source hook; it's checked in at mode 755 by git
# and mutating the tracked file from an installer would dirty the user's
# working tree.
echo "install-corrections-git-hook: installed $HOOK_TARGET -> $HOOK_SOURCE" >&2
