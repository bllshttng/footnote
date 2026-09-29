#!/usr/bin/env bash
# fno hook: Stop - compatibility path for older agy registrations
script_dir="${BASH_SOURCE[0]%/*}"
[[ "$script_dir" == "${BASH_SOURCE[0]}" ]] && script_dir="."
exec "$script_dir/footnote-agy-target-stop-hook.sh" "$@"
