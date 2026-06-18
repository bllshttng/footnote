#!/usr/bin/env bash
set -euo pipefail

repo_root() {
  local script_dir
  script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
  cd "$script_dir/../.." >/dev/null 2>&1
  pwd
}

check_dep() {
  local bin="$1"
  command -v "$bin" >/dev/null 2>&1
}

log_info() {
  printf '[INFO] %s\n' "$*"
}

log_warn() {
  printf '[WARN] %s\n' "$*"
}

log_err() {
  printf '[ERR] %s\n' "$*" >&2
}
