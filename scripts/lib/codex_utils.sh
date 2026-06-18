#!/usr/bin/env bash

codex_resolve_skills_root() {
  local root_dir="$1"
  local requested_root="${2:-}"
  local recorded_root_file="${3:-}"

  if [[ -n "$requested_root" ]]; then
    printf '%s\n' "$requested_root"
    return 0
  fi

  if [[ -n "$recorded_root_file" && -f "$recorded_root_file" ]]; then
    local recorded
    recorded="$(head -n 1 "$recorded_root_file" 2>/dev/null || true)"
    if [[ -n "$recorded" ]]; then
      printf '%s\n' "$recorded"
      return 0
    fi
  fi

  printf '%s/.agents/skills\n' "$root_dir"
}

codex_existing_parent_dir() {
  local path="$1"

  while [[ ! -e "$path" && "$path" != "/" ]]; do
    path="$(dirname -- "$path")"
  done

  if [[ -d "$path" ]]; then
    printf '%s\n' "$path"
  else
    dirname -- "$path"
  fi
}

codex_dir_is_writable_or_creatable() {
  local path="$1"

  if [[ -d "$path" ]]; then
    [[ -w "$path" ]]
    return
  fi

  local parent
  parent="$(codex_existing_parent_dir "$path")"
  [[ -d "$parent" && -w "$parent" ]]
}

codex_dir_supports_mutation() {
  local path="$1"

  if [[ -d "$path" ]]; then
    local probe="$path/.codex-write-probe.$$"
    if ! touch "$probe" 2>/dev/null; then
      return 1
    fi
    rm -f "$probe" 2>/dev/null || true
    return 0
  fi

  local parent
  parent="$(codex_existing_parent_dir "$path")"
  local probe_dir="$parent/.codex-mkdir-probe.$$"
  if ! mkdir "$probe_dir" 2>/dev/null; then
    return 1
  fi
  rmdir "$probe_dir" 2>/dev/null || true
  return 0
}
