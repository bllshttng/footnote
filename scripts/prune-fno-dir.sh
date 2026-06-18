#!/bin/bash
# prune-fno-dir.sh - janitor for stale files our pipeline leaves in ~/.fno/
# (stray graph copies, .bak files, dead recovery sentinels, superseded
# tasks.json). Disposition table was seeded by a 2026-04-22 inventory audit.
#
# Default mode is DRY-RUN: report what the script would archive or delete
# without touching anything. Pass --apply to actually perform the work.
#
# The audit also flagged "investigate" items that need user judgment
# before any code touches them (rotation policy for convo-signals.jsonl,
# the ~/.fno/.fno/ nested duplicate, the orphan SUMMARY.md,
# etc.). This script lists those items but does NOT modify them.
#
# Acceptance:
#   - Idempotent: running twice with --apply leaves the directory in the
#     same state as one run.
#   - Safe by default: dry-run mode is the default; --apply is required.
#   - No surprises: every action is printed to stdout BEFORE it happens.
#   - Bounded scope: only touches the disposition table from the audit.
#     Files outside that table are never read or written.
#
# Usage:
#   bash scripts/prune-fno-dir.sh           # dry-run report
#   bash scripts/prune-fno-dir.sh --apply   # perform archive + deletes
#   bash scripts/prune-fno-dir.sh --help

set -euo pipefail

FNO_DIR="${FNO_DIR:-${HOME}/.fno}"
ARCHIVE_DIR="${FNO_DIR}/archive/2026-04"

APPLY=0
FORCE=0
ARCHIVE_CONFLICTS=0

usage() {
  cat <<'USAGE'
prune-fno-dir.sh - apply 2026-04-22 inventory disposition table.

Default mode prints a dry-run report of pending archive + delete actions
for ~/.fno/. Pass --apply to perform the actions. Investigate items
are surfaced for user review and never touched by this script.

Options:
  --apply        Perform archive + delete actions. Default is dry-run.
  --force        Override the project-dir scope guard (see below). Only
                 meaningful with --apply.
  -h, --help     Print this help and exit.

Environment:
  FNO_DIR  Override the global abilities directory path (default
                 $HOME/.fno). Used by tests to point at a fixture.

Scope guard:
  The disposition table targets the GLOBAL ~/.fno only. If FNO_DIR points
  inside a git work tree (i.e. a project state dir), --apply refuses unless
  --force is given, so a stray FNO_DIR can never strip project state.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --apply)
      APPLY=1
      shift
      ;;
    --force)
      FORCE=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "prune-fno-dir.sh: unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ ! -d "$FNO_DIR" ]]; then
  echo "prune-fno-dir.sh: $FNO_DIR does not exist; nothing to do." >&2
  exit 1
fi

# Files dispositioned "delete" by the 2026-04-22 inventory audit. Each
# entry is a path relative to FNO_DIR. The script verifies a file
# exists before reporting; missing files are silently skipped to keep the
# script idempotent across repeated --apply runs.
DELETE_TARGETS=(
  "tasks.json"
  "tasks.md"
  "graph.json.bak"
  "graph copy.json"
  "graph.json.recovery-sentinel"
  "workspace.yaml.bak"
  "ledger.json.bak.pre-143-backfill"
  "do-target-stop-hook.log"
  ".DS_Store"
)

# Files dispositioned "archive" by the audit. Each entry is moved to
# ARCHIVE_DIR rather than deleted.
ARCHIVE_TARGETS=(
  "settings.yaml.bak"
)

# Items the audit flagged for human review before any cleanup. These are
# never touched by the script; they print as advisory output so the user
# remembers to handle them in a separate, more deliberate PR.
INVESTIGATE_NOTES=(
  "convo-signals.jsonl - 42 MB+; needs a rotation policy, not deletion"
  "signals/ - empty dir; confirm no future hook depends on it"
  "SUMMARY.md - March 20 orphan; verify no skill still reads it"
  ".fno/ (nested) - root-cause the hook that created the duplicate before pruning"
  "current-PLAN.md (per-project) - 0 refs in current code; older megawalk flows may still read it"
  "target-stop-hook.log (per-project) - unbounded growth; needs rotation in stop hook, not deletion"
)

# is_project_fno_dir: true when FNO_DIR is a project state dir (lives inside a
# git work tree) rather than the global ~/.fno. The global dir is always
# allowed, even when $HOME itself happens to be a git repo (dotfiles).
is_project_fno_dir() {
  local canon home_fno
  canon="$(cd "$FNO_DIR" 2>/dev/null && pwd -P)" || return 1
  home_fno="$(cd "$HOME/.fno" 2>/dev/null && pwd -P)" || home_fno=""
  [[ -n "$home_fno" && "$canon" == "$home_fno" ]] && return 1
  git -C "$canon" rev-parse --is-inside-work-tree >/dev/null 2>&1
}

print_header() {
  if [[ $APPLY -eq 1 ]]; then
    echo "prune-fno-dir.sh: APPLY mode (will archive + delete)"
  else
    echo "prune-fno-dir.sh: dry-run (no files modified)"
  fi
  echo "  target dir: $FNO_DIR"
  echo "  archive dir: $ARCHIVE_DIR"
  echo
}

ensure_archive_dir() {
  if [[ $APPLY -eq 1 ]]; then
    mkdir -p "$ARCHIVE_DIR"
  fi
}

report_archive() {
  local count=0
  echo "ARCHIVE candidates (move to $ARCHIVE_DIR):"
  for rel in "${ARCHIVE_TARGETS[@]}"; do
    local src="$FNO_DIR/$rel"
    if [[ -e "$src" ]]; then
      count=$((count + 1))
      echo "  - $rel"
      if [[ $APPLY -eq 1 ]]; then
        local dest="$ARCHIVE_DIR/$rel"
        if [[ -e "$dest" ]]; then
          # If both source and destination exist, reconcile only when the
          # contents are identical: drop the source so a re-run leaves a
          # clean state. When the contents differ we refuse to clobber and
          # flag the conflict for manual review - silently leaving the
          # source behind would break the script's idempotency claim and
          # leak the file we promised to archive.
          if cmp -s "$src" "$dest"; then
            rm -f "$src"
            echo "    reconciled: dest already had identical bytes; removed source $src"
          else
            ARCHIVE_CONFLICTS=$((ARCHIVE_CONFLICTS + 1))
            echo "    CONFLICT: $dest exists with different bytes; leaving source $src in place" >&2
          fi
        else
          mv "$src" "$dest"
          echo "    moved: $src -> $dest"
        fi
      fi
    fi
  done
  if [[ $count -eq 0 ]]; then
    echo "  (none present; already pruned or never existed)"
  fi
  echo
}

report_delete() {
  local count=0
  echo "DELETE candidates:"
  for rel in "${DELETE_TARGETS[@]}"; do
    local target="$FNO_DIR/$rel"
    if [[ -e "$target" ]]; then
      count=$((count + 1))
      echo "  - $rel"
      if [[ $APPLY -eq 1 ]]; then
        rm -f "$target"
        echo "    deleted: $target"
      fi
    fi
  done
  if [[ $count -eq 0 ]]; then
    echo "  (none present; already pruned or never existed)"
  fi
  echo
}

report_investigate() {
  echo "INVESTIGATE (not touched by this script):"
  for note in "${INVESTIGATE_NOTES[@]}"; do
    echo "  - $note"
  done
  echo
}

main() {
  print_header
  if is_project_fno_dir; then
    if [[ $APPLY -eq 1 && $FORCE -eq 0 ]]; then
      echo "prune-fno-dir.sh: REFUSING --apply: $FNO_DIR is inside a git" >&2
      echo "  work tree (a project state dir), not the global ~/.fno. The" >&2
      echo "  disposition table is for global cleanup; applying it here could" >&2
      echo "  remove project state. Re-run with --force only if you are certain." >&2
      exit 4
    fi
    echo "WARNING: $FNO_DIR looks like a project state dir (inside a git repo)."
    echo "  This table is meant for the global ~/.fno; --apply is guarded here."
    echo
  fi
  ensure_archive_dir
  report_archive
  report_delete
  report_investigate
  if [[ $APPLY -eq 1 ]]; then
    if [[ $ARCHIVE_CONFLICTS -gt 0 ]]; then
      echo "prune-fno-dir.sh: apply finished with $ARCHIVE_CONFLICTS conflict(s) - resolve manually." >&2
      exit 3
    fi
    echo "prune-fno-dir.sh: apply complete."
  else
    echo "prune-fno-dir.sh: re-run with --apply to perform the actions above."
  fi
}

main
