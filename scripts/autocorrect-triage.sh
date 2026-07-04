#!/usr/bin/env bash
# autocorrect-triage.sh - interactive walk-through of proposed patches.
#
# Reads the latest patch file from ~/.claude/proposed-patches/{review_id}.md
# (or --review-id <id> for a specific one), parses the numbered items,
# and prompts the user accept/reject/defer/skip/quit for each.
#
# On accept: applies the patch. If the patch contains a unified diff inside
# a ```diff fence, runs git apply. Otherwise prints the proposed text and
# tells the user where to apply it manually.
#
# On reject: appends to ~/.fno/corrections-rejected.log so subsequent
# reviewers don't propose the same change.
#
# On CONVERT-TO-VERIFIER (action c): requires the patch to mention both
# the verifier creation and the rule deletion. The actual atomic-commit
# enforcement is the user's responsibility; the script surfaces the
# invariant prominently.
#
# After all patches are processed, prompts to create a single commit
# bundling the accepted patches.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/corrections-lock.sh
source "$SCRIPT_DIR/lib/corrections-lock.sh"

CLAUDE_DIR="${CLAUDE_DIR_OVERRIDE:-$HOME/.claude}"
PATCHES_DIR="$CLAUDE_DIR/proposed-patches"
REJECTED_LOG="$(corrections_rejected_log_path)"
MALFORMED_LOG="$CLAUDE_DIR/corrections-malformed.log"

REVIEW_ID=""
PATCH_FILE=""
AUTO=""  # for non-interactive use during smoke tests; accepts "defer-all"
TARGET_DIR=""

# Allowlist of repo-relative path prefixes a patch may touch. Refusing
# anything else closes the "reviewer-proposed patch targets
# ~/.ssh/authorized_keys" defense-in-depth gap.
_PATCH_PATH_ALLOWLIST=(
  "rules/"
  "skills/"
  "CLAUDE.md"
  "GEMINI.md"
  "AGENTS.md"
  "commands/"
  "agents/"
  "hooks/"
)

usage() {
  cat >&2 <<'EOF'
Usage: autocorrect-triage.sh [--review-id <id>] [--patch-file <path>] [--auto defer-all]

Walks the proposed-patches list interactively. Each patch prompts:
  [a]ccept   apply the patch (git apply if diff-shaped, else print)
  [r]eject   record rejection in corrections-rejected.log
  [d]efer    leave for the next review
  [s]kip     no decision this round
  [q]uit     stop processing

Options:
  --review-id <id>      use a specific review (default: most recent)
  --patch-file <path>   explicit path to a patch file
  --target-dir <path>   git repo where patches are applied (default: $HOME/.claude)
  --auto defer-all      non-interactive: defer every patch (for testing)
EOF
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --review-id)  REVIEW_ID="${2:-}"; shift 2 ;;
    --patch-file) PATCH_FILE="${2:-}"; shift 2 ;;
    --target-dir) TARGET_DIR="${2:-}"; shift 2 ;;
    --auto)       AUTO="${2:-}"; shift 2 ;;
    -h|--help)    usage ;;
    *) echo "autocorrect-triage: unknown argument: $1" >&2; usage ;;
  esac
done

# Resolve the target directory where patches will be applied. Patches the
# reviewer produces are intended to land in ~/.claude/ (rules, skills,
# CLAUDE.md, etc), so default there. The user can override via flag.
if [[ -z "$TARGET_DIR" ]]; then
  TARGET_DIR="$CLAUDE_DIR"
fi
if [[ ! -d "$TARGET_DIR" ]]; then
  echo "autocorrect-triage: target dir does not exist: $TARGET_DIR" >&2
  exit 1
fi
if [[ ! -d "$TARGET_DIR/.git" ]]; then
  echo "autocorrect-triage: target dir is not a git repo: $TARGET_DIR" >&2
  echo "autocorrect-triage: patches must apply against a git checkout" >&2
  exit 1
fi

# Resolve which patch file to read.
if [[ -z "$PATCH_FILE" ]]; then
  if [[ -n "$REVIEW_ID" ]]; then
    PATCH_FILE="$PATCHES_DIR/${REVIEW_ID}.md"
  else
    if [[ ! -d "$PATCHES_DIR" ]]; then
      echo "autocorrect-triage: no proposed-patches directory at $PATCHES_DIR" >&2
      exit 1
    fi
    # Most recent.
    PATCH_FILE=$(find "$PATCHES_DIR" -maxdepth 1 -type f -name "*.md" -print 2>/dev/null \
      | sort -r | head -1)
    if [[ -z "$PATCH_FILE" ]]; then
      echo "autocorrect-triage: no patch files in $PATCHES_DIR" >&2
      exit 1
    fi
  fi
fi

if [[ ! -f "$PATCH_FILE" ]]; then
  echo "autocorrect-triage: patch file not found: $PATCH_FILE" >&2
  exit 1
fi

echo "autocorrect-triage: reading $PATCH_FILE" >&2

# Split patch file into per-item chunks. Each item starts with "N. Source:" line.
TMPDIR_T="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_T"' EXIT
ITEMS_DIR="$TMPDIR_T/items"
mkdir -p "$ITEMS_DIR"

awk -v dir="$ITEMS_DIR" '
  /^[0-9]+\. Source:/ {
    if (current) close(current);
    item_num = $1
    sub(/\.$/, "", item_num)
    current = dir "/item-" sprintf("%04d", item_num) ".txt"
  }
  current { print > current }
' "$PATCH_FILE"

ITEMS=()
while IFS= read -r f; do
  ITEMS+=("$f")
done < <(find "$ITEMS_DIR" -maxdepth 1 -type f -name "item-*.txt" 2>/dev/null | sort)

if [[ "${#ITEMS[@]}" -eq 0 ]]; then
  echo "autocorrect-triage: no parseable items in $PATCH_FILE" >&2
  echo "autocorrect-triage: the file may use a different format than expected" >&2
  exit 1
fi

ACCEPTED_PATCHES=()
QUIT=0

# Check every `--- a/X` / `+++ b/X` path in the diff against the allowlist.
# Refuses patches that target paths outside the allowed prefixes. Defense
# in depth against a compromised API key or prompt-injection-crafted diff.
_diff_paths_allowed() {
  local diff_path="$1"
  local violations=0
  while IFS= read -r path; do
    [[ -z "$path" ]] && continue
    local matched=0
    for prefix in "${_PATCH_PATH_ALLOWLIST[@]}"; do
      if [[ "$path" == "$prefix"* ]]; then
        matched=1
        break
      fi
    done
    if [[ "$matched" == "0" ]]; then
      echo "autocorrect-triage: refusing patch path outside allowlist: $path" >&2
      violations=$((violations + 1))
    fi
  done < <(awk '/^(--- a\/|\+\+\+ b\/)/{sub(/^[+-]+ [ab]\//, ""); print}' "$diff_path" | sort -u)
  [[ "$violations" -eq 0 ]]
}

# Stage exactly the files modified by the diff into TARGET_DIR's index.
# Returns the list of files on stdout for the caller to commit-message.
_files_in_diff() {
  local diff_path="$1"
  awk '/^\+\+\+ b\//{sub(/^\+\+\+ b\//, ""); print}' "$diff_path" | sort -u
}

apply_diff_block() {
  local item_file="$1"
  # Extract content of ```diff ... ``` fence.
  local tmp_diff="$TMPDIR_T/proposed.diff"
  awk '
    /^```diff/ {in_block=1; next}
    /^```/ && in_block {in_block=0; exit}
    in_block {print}
  ' "$item_file" > "$tmp_diff"
  if [[ ! -s "$tmp_diff" ]]; then
    return 1  # no diff block
  fi
  # Allowlist gate. A patch that touches paths outside rules/ skills/ etc
  # is treated as malformed and logged.
  if ! _diff_paths_allowed "$tmp_diff"; then
    local ts
    ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    printf '%s | %s | path outside allowlist\n' "$ts" "$(basename "$item_file")" >> "$MALFORMED_LOG"
    return 1
  fi
  # Apply inside TARGET_DIR so the patch lands in the intended repo
  # regardless of where the user launched triage from.
  if ( cd "$TARGET_DIR" && git apply --check "$tmp_diff" 2>/dev/null ); then
    if ( cd "$TARGET_DIR" && git apply "$tmp_diff" ); then
      # Stage only the files the patch actually touched so unrelated
      # changes in the user's working tree are not swept into the commit.
      local touched_files=()
      while IFS= read -r p; do
        [[ -n "$p" ]] && touched_files+=("$p")
      done < <(_files_in_diff "$tmp_diff")
      if [[ "${#touched_files[@]}" -gt 0 ]]; then
        ( cd "$TARGET_DIR" && git add -- "${touched_files[@]}" )
      fi
      echo "autocorrect-triage: applied diff to $TARGET_DIR" >&2
      return 0
    fi
  fi
  # Malformed or conflicting.
  local ts
  ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  printf '%s | %s | apply failed\n' "$ts" "$(basename "$item_file")" >> "$MALFORMED_LOG"
  echo "autocorrect-triage: patch did not apply cleanly; logged to $MALFORMED_LOG" >&2
  return 1
}

handle_convert_to_verifier_invariant() {
  local item_file="$1"
  # Detect Action: c. Convert-to-verifier MUST include both the verifier
  # text and the rule deletion. Heuristic: the item should mention "delete"
  # AND "verifier" or "script" in its body.
  if ! grep -qiE 'delete|removal|remove' "$item_file"; then
    echo "" >&2
    echo "WARNING: CONVERT-TO-VERIFIER patch does not mention rule deletion." >&2
    echo "The invariant requires deleting the rule text in the same commit as" >&2
    echo "adding the verifier. This patch may be incomplete." >&2
    return 1
  fi
  return 0
}

for item_file in "${ITEMS[@]}"; do
  [[ "$QUIT" == "1" ]] && break

  echo "----------------------------------------"
  cat "$item_file"
  echo "----------------------------------------"

  ACTION=$(grep -E '^[[:space:]]*Action:' "$item_file" | head -1 | sed -E 's/^[[:space:]]*Action:[[:space:]]*//' | tr '[:upper:]' '[:lower:]' | head -c 1)
  if [[ "$ACTION" == "c" ]]; then
    if ! handle_convert_to_verifier_invariant "$item_file"; then
      echo "Auto-deferring this patch pending a clarifying revision." >&2
      continue
    fi
  fi

  # Interactive prompt.
  if [[ "$AUTO" == "defer-all" ]]; then
    REPLY="d"
  else
    printf "Action [a/r/d/s/q] " >&2
    if ! read -r REPLY; then
      REPLY="q"
    fi
  fi

  case "${REPLY:0:1}" in
    a|A)
      if apply_diff_block "$item_file"; then
        ACCEPTED_PATCHES+=("$(basename "$item_file")")
      else
        echo "autocorrect-triage: no clean diff block found; manual apply required" >&2
        cat "$item_file" >&2
      fi
      ;;
    r|R)
      TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
      SOURCE_LINE=$(grep -E '^[0-9]+\. Source:' "$item_file" | head -1)
      mkdir -p "$(dirname "$REJECTED_LOG")"
      printf '%s | rejected | %s\n' "$TIMESTAMP" "$SOURCE_LINE" >> "$REJECTED_LOG"
      chmod 600 "$REJECTED_LOG" 2>/dev/null || true
      echo "autocorrect-triage: rejected; logged to $REJECTED_LOG" >&2
      ;;
    d|D)
      echo "autocorrect-triage: deferred" >&2
      ;;
    s|S)
      echo "autocorrect-triage: skipped (no decision)" >&2
      ;;
    q|Q)
      QUIT=1
      echo "autocorrect-triage: quit" >&2
      ;;
    *)
      echo "autocorrect-triage: unknown action '$REPLY'; deferring" >&2
      ;;
  esac
done

# Final commit prompt.
if [[ "${#ACCEPTED_PATCHES[@]}" -gt 0 ]]; then
  echo ""
  echo "autocorrect-triage: ${#ACCEPTED_PATCHES[@]} patch(es) accepted and staged." >&2
  if [[ "$AUTO" == "defer-all" ]]; then
    REPLY="n"
  else
    printf "Commit accepted patches as a single commit? [y/N] " >&2
    if ! read -r REPLY; then
      REPLY="n"
    fi
  fi
  if [[ "${REPLY:0:1}" =~ [yY] ]]; then
    REVIEW_ID_FROM_FILE=$(basename "$PATCH_FILE" .md)
    # Files were staged per-patch in apply_diff_block; commit just those.
    # No `git add -A` so the user's unrelated work-in-progress is safe.
    ( cd "$TARGET_DIR" && git commit -m "autocorrect ${REVIEW_ID_FROM_FILE}: applied ${#ACCEPTED_PATCHES[@]} patches" )
    echo "autocorrect-triage: committed in $TARGET_DIR" >&2
  else
    echo "autocorrect-triage: leaving changes staged in $TARGET_DIR for manual commit" >&2
  fi
fi

echo "autocorrect-triage: done" >&2
