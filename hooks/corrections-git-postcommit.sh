#!/usr/bin/env bash
# corrections-git-postcommit.sh - capture rule edits in ~/.claude/ to corrections.log.
#
# Installed as the post-commit hook in ~/.claude/.git/hooks/ by
# scripts/install-corrections-git-hook.sh. Fires on every commit to ~/.claude/,
# inspects which files changed, and emits one corrections.log line per
# instruction-bearing file edited.
#
# Severity is S0 if the commit message contains "urgent:" or "revert:".
# Severity is S1 otherwise (default for rule edits).
#
# Output is stderr-only on the hook side; never blocks the commit.

set -euo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

# Resolve our shared helpers. The hook is normally invoked from ~/.claude/.git/hooks/post-commit
# which is a symlink to this file in the fno repo; resolve through the symlink.
SCRIPT_PATH="$(readlink -f "${BASH_SOURCE[0]}" 2>/dev/null || stat -f %Y "${BASH_SOURCE[0]}" 2>/dev/null || echo "${BASH_SOURCE[0]}")"
SCRIPT_DIR="$(cd "$(dirname "$SCRIPT_PATH")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

LOCK_HELPER="$REPO_ROOT/scripts/lib/corrections-lock.sh"
if [[ ! -f "$LOCK_HELPER" ]]; then
  echo "corrections-git-postcommit: lock helper not found at $LOCK_HELPER" >&2
  exit 0  # never block the commit
fi
# shellcheck source=/dev/null
source "$LOCK_HELPER"

LOG_PATH="$(corrections_log_path)"
if [[ ! -f "$LOG_PATH" ]]; then
  # No log = no consumer cares yet. Don't bootstrap from a hook; that's
  # the install script's job.
  exit 0
fi

# Read the just-committed change set. git show --name-only handles
# both root commits (no parent to diff against) and normal commits,
# unlike git diff-tree which returns empty for root commits.
CHANGED_FILES=$(git show --name-only --pretty=format: HEAD 2>/dev/null | sed '/^$/d' || echo "")
COMMIT_SUBJECT=$(git log -1 --pretty=%s 2>/dev/null || echo "")
TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

# Severity from commit message.
SEVERITY="S1"
case "$COMMIT_SUBJECT" in
  urgent:*|revert:*) SEVERITY="S0" ;;
esac

# SOURCE from the committing repo: the rules corpus in ~/.claude
# stays git-rule-edit; a shipped skill or rule edited in its own repo is a
# skill-commit, the row corrections-verify scores a hand-applied correction
# against. One toplevel comparison, no second hook.
CLAUDE_REPO="${CLAUDE_DIR_OVERRIDE:-$HOME/.claude}"
COMMIT_REPO="$(git rev-parse --show-toplevel 2>/dev/null || echo "")"
SOURCE_FIELD="skill-commit"
# pwd -P on both sides: macOS git reports /private/var where the caller
# wrote /var (same directory, two spellings), and a lexical compare would
# misfile every row.
if [[ -n "$COMMIT_REPO" ]]; then
  COMMIT_REPO="$(cd "$COMMIT_REPO" 2>/dev/null && pwd -P || printf '%s' "$COMMIT_REPO")"
  CLAUDE_REPO="$(cd "$CLAUDE_REPO" 2>/dev/null && pwd -P || printf '%s' "$CLAUDE_REPO")"
  if [[ "$COMMIT_REPO" == "$CLAUDE_REPO" ]]; then
    SOURCE_FIELD="git-rule-edit"
  fi
fi

# Filter to instruction-bearing files. The set is intentionally narrow so
# editing arbitrary files in ~/.claude (e.g. transcripts, caches) doesn't
# flood the log.
emit_for() {
  local file="$1"
  # In bash case-glob matching `*` matches `/`, so `skills/*/SKILL.md`
  # covers SKILL.md at any depth under skills/. One pattern, no redundant
  # variants.
  case "$file" in
    rules/*.md) ;;
    skills/*/SKILL.md) ;;
    CLAUDE.md|GEMINI.md|AGENTS.md) ;;
    plugins/*/CLAUDE.md|plugins/*/GEMINI.md) ;;
    *) return 1 ;;
  esac
  local details
  details="$(corrections_escape_details "$COMMIT_SUBJECT")"
  local line="${TIMESTAMP} | ${SEVERITY} | ${SOURCE_FIELD} | ${file} | ${details}"
  corrections_lock_append "$LOG_PATH" "$line" || \
    echo "corrections-git-postcommit: failed to write entry for $file" >&2
  return 0
}

while IFS= read -r file; do
  [[ -z "$file" ]] && continue
  emit_for "$file" || true
done <<< "$CHANGED_FILES"

exit 0
