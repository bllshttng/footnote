#!/bin/bash
# Git checkpoint management for target
# Provides stash-based checkpoints before risky phases
#
# Usage:
#   source scripts/lib/checkpoint.sh
#   CHECKPOINT=$(create_checkpoint "execute" "2")
#   rollback_checkpoint "target-checkpoint-execute-wave-2"
#   list_checkpoints
#   cleanup_checkpoints 3

CHECKPOINT_PREFIX="target-checkpoint"

# Create a named checkpoint (git stash)
# Args: $1=phase $2=wave (default 0)
# Output: "ref:name" or "name:clean" if nothing to stash
create_checkpoint() {
  local phase="$1"
  local wave="${2:-0}"
  local name="${CHECKPOINT_PREFIX}-${phase}-wave-${wave}"

  # Only checkpoint if there are changes to stash
  if git diff --quiet && git diff --cached --quiet && \
     [ -z "$(git ls-files --others --exclude-standard)" ]; then
    echo "${name}:clean"
    return 0
  fi

  # Stash including untracked files
  if ! git stash push -u -m "$name" 2>/dev/null; then
    echo "ERROR: git stash push failed for '$name'" >&2
    return 1
  fi
  local ref
  ref=$(git stash list | grep -w "$name" | head -1 | cut -d: -f1)
  if [ -z "$ref" ]; then
    echo "ERROR: stash created but ref not found for '$name'" >&2
    return 1
  fi
  echo "${ref}:${name}"
}

# Rollback to a named checkpoint
# Args: $1=checkpoint name
# Returns: 0 on success, 1 if checkpoint not found
rollback_checkpoint() {
  local name="$1"
  local ref
  ref=$(git stash list | grep -w "$name" | head -1 | cut -d: -f1)

  if [ -z "$ref" ]; then
    echo "ERROR: checkpoint '$name' not found" >&2
    return 1
  fi

  # Reset working tree to clean state, then apply stash
  # Use apply+drop instead of pop so checkpoint survives on failure
  git reset --hard HEAD 2>/dev/null
  if ! git stash apply "$ref" 2>/dev/null; then
    echo "ERROR: stash apply failed (possible merge conflict) for '$name'" >&2
    echo "Checkpoint preserved at $ref - retry or drop manually" >&2
    return 1
  fi
  git stash drop "$ref" 2>/dev/null
  echo "Rolled back to checkpoint: $name"
}

# List all target checkpoints
list_checkpoints() {
  git stash list | grep "$CHECKPOINT_PREFIX"
}

# Clean up old checkpoints (keep last N)
# Args: $1=number to keep (default 3)
cleanup_checkpoints() {
  local keep="${1:-3}"
  local count=0
  local names_to_drop=()

  # Collect checkpoint NAMES to drop (not indices, which go stale)
  while IFS= read -r line; do
    count=$((count + 1))
    if [ "$count" -gt "$keep" ]; then
      # Extract the stash message (checkpoint name) from the stash entry
      local msg
      msg=$(echo "$line" | sed 's/^[^:]*: [^:]*: //')
      names_to_drop+=("$msg")
    fi
  done < <(git stash list | grep "$CHECKPOINT_PREFIX")

  # Drop by re-resolving name to ref each time (indices shift after each drop)
  local dropped=0
  for name in "${names_to_drop[@]}"; do
    local ref
    ref=$(git stash list | grep -w "$name" | head -1 | cut -d: -f1)
    if [ -n "$ref" ]; then
      git stash drop "$ref" 2>/dev/null
      dropped=$((dropped + 1))
    fi
  done

  if [ "$dropped" -gt 0 ]; then
    echo "Cleaned up ${dropped} old checkpoint(s)"
  fi
}
