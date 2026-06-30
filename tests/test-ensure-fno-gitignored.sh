#!/usr/bin/env bash
# Runnable check for hooks/helpers/ensure-fno-gitignored.sh
set -uo pipefail
HELPER="$(cd "$(dirname "$0")/.." && pwd)/hooks/helpers/ensure-fno-gitignored.sh"

tmp=$(mktemp -d); tmp2=$(mktemp -d)
trap 'rm -rf "$tmp" "$tmp2"' EXIT
git -C "$tmp" init -q
mkdir "$tmp/.fno"

# 1. appends the rule when .fno/ exists and is not yet ignored
bash "$HELPER" "$tmp"
git -C "$tmp" check-ignore -q .fno || { echo "FAIL: .fno not ignored after run"; exit 1; }

# 2. idempotent: a second run does not duplicate the rule
bash "$HELPER" "$tmp"
count=$(grep -c '^\.fno/$' "$tmp/.gitignore")
[[ "$count" -eq 1 ]] || { echo "FAIL: rule written $count times, want 1"; exit 1; }

# 3. no-op when there is no .fno/ (don't create a .gitignore out of nowhere)
git -C "$tmp2" init -q
bash "$HELPER" "$tmp2"
[[ ! -f "$tmp2/.gitignore" ]] || { echo "FAIL: wrote .gitignore with no .fno/"; exit 1; }

echo "PASS"
