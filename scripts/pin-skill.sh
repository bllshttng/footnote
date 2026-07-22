#!/usr/bin/env bash
# pin-skill.sh - create / remove top-level shortcut "pin" skills that redirect
# to a cluster router mode (epic ab-0d05a9b7, group 5: front-door + pinning).
#
# A pin is an OPT-IN, repo-level shortcut. `pin sigma --to "review sigma"`
# writes skills/sigma/SKILL.md whose body tells the agent to invoke
# `/review sigma`. Every generated shim carries a marker comment so `unpin`
# only ever removes generated shims, never a user-authored skill of the same
# name (the impeccable pin precedent; AC6-FR).
#
# Usage:
#   scripts/pin-skill.sh pin   <name> --to "<verb> [mode]" [--desc "text"] [--replace]
#   scripts/pin-skill.sh unpin <name>
#   scripts/pin-skill.sh list
#
# --replace lets `pin` overwrite an EXISTING non-pin skill's SKILL.md - the
# one-release deprecation path (redirect an old top-level name to its router
# form). It writes only SKILL.md, preserving sibling files (scripts/,
# references/) that the bundler may still source; it warns when the target
# folder holds such files, because `unpin` removes the whole folder.
#
# The shim body uses a slash-command redirect in prose (never a Skill() call),
# so it does not reintroduce the runtime skill-to-skill calls the encapsulation
# invariants forbid.
set -euo pipefail

readonly PIN_MARKER='<!-- fno-pinned-skill -->'

die() { echo "pin-skill: $*" >&2; exit 1; }

usage() {
  cat <<'EOF'
pin-skill.sh - create / remove top-level shortcut "pin" skills that redirect
to a cluster router mode (epic ab-0d05a9b7, group 5).

Usage:
  scripts/pin-skill.sh pin   <name> --to "<verb> [mode]" [--desc "text"] [--replace]
  scripts/pin-skill.sh unpin <name>
  scripts/pin-skill.sh list

A pin writes skills/<name>/SKILL.md as a prose redirect to /<verb> [mode]
(never a Skill() call), carrying the marker <!-- fno-pinned-skill -->
so unpin only ever removes generated shims, never a user-authored skill of
the same name. --replace overwrites an existing non-pin skill's SKILL.md
(the one-release deprecation path), preserving sibling files the bundler may
still source.
EOF
}

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || die "not in a git repo"
readonly ROOT
readonly SKILLS_DIR="$ROOT/skills"

render_shim() {
  # $1 name, $2 target (e.g. "review sigma"), $3 description
  local name="$1" target="$2" desc="$3"
  cat <<EOF
---
name: ${name}
description: "${desc}"
user-invocable: true
---

${PIN_MARKER}

This is a generated shortcut for \`/${target}\`.

Invoke \`/${target}\`, passing along any arguments given here, and follow its
instructions. Do not reimplement its behavior in this skill.
EOF
}

cmd_pin() {
  local name="${1:-}" target="" desc="" replace=0
  [[ -n "$name" ]] || die "pin: missing <name>"
  shift
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --to)
        [[ $# -ge 2 ]] || die "pin: --to needs a value"
        [[ "$2" != --* ]] || die "pin: --to value looks like a flag ('$2')"
        target="$2"; shift 2 ;;
      --desc)
        [[ $# -ge 2 ]] || die "pin: --desc needs a value"
        [[ "$2" != --* ]] || die "pin: --desc value looks like a flag ('$2')"
        desc="$2"; shift 2 ;;
      --replace) replace=1; shift ;;
      *)         die "pin: unknown flag '$1'" ;;
    esac
  done
  [[ -n "$target" ]] || die "pin: --to \"<verb> [mode]\" required"
  [[ "$name" =~ ^[a-z][a-z0-9_-]*$ ]] || die "pin: name must match [a-z][a-z0-9_-]*"
  [[ "$desc" != *'"'* ]] || die "pin: --desc must not contain a double-quote"
  [[ -n "$desc" ]] || desc="Shortcut for /${target}."

  local skill_dir="$SKILLS_DIR/$name" md
  md="$skill_dir/SKILL.md"
  if [[ -d "$skill_dir" ]]; then
    local is_pin=0
    if [[ -f "$md" ]] && grep -qxF "$PIN_MARKER" "$md"; then
      is_pin=1  # existing pin - idempotent overwrite (re-pin)
    fi
    # A non-pin SKILL.md may only be overwritten with --replace (deprecation).
    if [[ $is_pin -eq 0 && -f "$md" && $replace -ne 1 ]]; then
      die "skills/$name exists and is not a pin. Pass --replace to deprecate it (redirect to /$target)."
    fi
    # Whenever we plant a pin over a pre-existing (non-pin) folder - whether
    # it has a non-pin SKILL.md or no SKILL.md at all - note any sibling files
    # (bundler-sourced plumbing). `unpin` preserves them (it removes only
    # SKILL.md and rmdir's the folder only when empty), so this is
    # informational, not a danger.
    if [[ $is_pin -eq 0 ]]; then
      local others
      others=$(find "$skill_dir" -type f ! -name SKILL.md 2>/dev/null | wc -l | tr -d ' ')
      if [[ "$others" -gt 0 ]]; then
        echo "pin-skill: note: skills/$name holds $others non-SKILL.md file(s) the bundler may source; they are preserved on unpin (only SKILL.md is removed)." >&2
      fi
    fi
  fi
  mkdir -p "$skill_dir"
  render_shim "$name" "$target" "$desc" > "$md"
  echo "pin-skill: pinned /$name -> /$target"
}

cmd_unpin() {
  local name="${1:-}"
  [[ -n "$name" ]] || die "unpin: missing <name>"
  # Validate before constructing any path: an unvalidated name like '..' or
  # '../foo' would resolve outside skills/ and rm a marked dir there.
  [[ "$name" =~ ^[a-z][a-z0-9_-]*$ ]] || die "unpin: name must match [a-z][a-z0-9_-]* (refusing path traversal)"
  local skill_dir="$SKILLS_DIR/$name" md="$SKILLS_DIR/$name/SKILL.md"
  if [[ ! -e "$skill_dir" ]]; then
    echo "pin-skill: no skill named '$name'"
    return 0
  fi
  [[ -f "$md" ]] || die "skills/$name has no SKILL.md; refusing to touch it"
  # Whole-line match (-x): a user-authored skill that merely quotes the marker
  # string inline in its prose must NOT be treated as a generated pin (AC6-FR).
  if ! grep -qxF "$PIN_MARKER" "$md"; then
    die "skills/$name is not a pin (no marker); refusing to remove a user-authored skill"
  fi
  # Remove only the generated shim. rmdir succeeds only on an otherwise-empty
  # folder (a plain net-new pin); a folder that still holds sibling files
  # (bundler-sourced plumbing from a --replace deprecation) is kept intact.
  rm -f "$md"
  if rmdir "$skill_dir" 2>/dev/null; then
    echo "pin-skill: unpinned /$name (removed skills/$name)"
  else
    echo "pin-skill: unpinned /$name (removed SKILL.md; kept skills/$name - it holds other files)"
  fi
}

cmd_list() {
  local found=0 md
  shopt -s nullglob
  for md in "$SKILLS_DIR"/*/SKILL.md; do
    if grep -qxF "$PIN_MARKER" "$md"; then
      echo "  /$(basename "$(dirname "$md")")"
      found=1
    fi
  done
  shopt -u nullglob
  [[ $found -eq 1 ]] || echo "pin-skill: no pinned shims."
}

case "${1:-}" in
  pin)          shift; cmd_pin "$@" ;;
  unpin)        shift; cmd_unpin "$@" ;;
  list)         shift; cmd_list "$@" ;;
  ""|-h|--help) usage ;;
  *)            die "unknown action '${1}'. Use pin|unpin|list (or --help)." ;;
esac
