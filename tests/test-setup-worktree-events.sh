#!/usr/bin/env bash

set -uo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel)
SETUP="$REPO_ROOT/scripts/setup/setup-worktree.sh"
TMP=$(mktemp -d -t setup-worktree-events-XXXXXX)
trap 'rm -rf "$TMP"' EXIT
fail=0

assert() {
  local label="$1"
  shift
  if "$@"; then
    echo "PASS: $label"
  else
    echo "FAIL: $label"
    fail=1
  fi
}

canonical="$TMP/canonical"
fresh="$TMP/fresh"
mkdir -p "$canonical/.fno" "$fresh"
printf '%s' '{"type":"canonical_before"}' > "$canonical/.fno/events.jsonl"

CANONICAL="$canonical" WORKTREE="$fresh" bash "$SETUP" >/dev/null 2>&1
assert "fresh worktree gets an events symlink" test -L "$fresh/.fno/events.jsonl"
assert "fresh symlink targets the canonical journal" test "$canonical/.fno/events.jsonl" -ef "$fresh/.fno/events.jsonl"
resolved_shell_path=$(
  EVENTS_FILE="$fresh/.fno/events.jsonl"
  # shellcheck disable=SC1090
  source "$REPO_ROOT/scripts/lib/events.sh"
  printf '%s' "$EVENTS_FILE"
)
assert "shell writers resolve the shared target path" test "$resolved_shell_path" -ef "$canonical/.fno/events.jsonl"

existing="$TMP/existing"
mkdir -p "$existing/.fno"
printf '%s' '{"type":"worktree_before"}' > "$existing/.fno/events.jsonl"

CANONICAL="$canonical" WORKTREE="$existing" bash "$SETUP" >/dev/null 2>&1
assert "existing real journal is replaced by a symlink" test -L "$existing/.fno/events.jsonl"
assert "migrated symlink targets the canonical journal" test "$canonical/.fno/events.jsonl" -ef "$existing/.fno/events.jsonl"
assert "canonical bytes survive migration" grep -q '"type":"canonical_before"' "$canonical/.fno/events.jsonl"
assert "worktree bytes reach canonical journal" grep -q '"type":"worktree_before"' "$canonical/.fno/events.jsonl"
assert "unterminated inputs become two complete rows" test "$(wc -l < "$canonical/.fno/events.jsonl" | tr -d ' ')" -eq 2
while IFS= read -r row; do
  assert "migrated row remains valid JSON" jq -e . <<< "$row"
done < "$canonical/.fno/events.jsonl"

shopt -s nullglob
backups=("$existing/.fno/events.jsonl.pre-share."*)
shopt -u nullglob
assert "migration retains one pre-share backup" test "${#backups[@]}" -eq 1
if (( ${#backups[@]} == 1 )); then
  assert "backup retains the worktree bytes" grep -q '"type":"worktree_before"' "${backups[0]}"
fi

contended="$TMP/contended"
mkdir -p "$contended/.fno"
printf '%s\n' '{"type":"before_lock"}' > "$contended/.fno/events.jsonl"
(
  lock_dir="$contended/.fno/events.jsonl.lock.d"
  mkdir "$lock_dir"
  printf '%s' "test:$$:holder" > "$lock_dir/owner"
  printf '%s' ready > "$contended/holder-ready"
  sleep 0.2
  printf '%s\n' '{"type":"during_lock"}' >> "$contended/.fno/events.jsonl"
  rm -f "$lock_dir/owner"
  rmdir "$lock_dir"
) &
holder_pid=$!
for _ in {1..100}; do
  [[ -f "$contended/holder-ready" ]] && break
  sleep 0.01
done
assert "writer lock holder started" test -f "$contended/holder-ready"
CANONICAL="$canonical" WORKTREE="$contended" bash "$SETUP" >/dev/null 2>&1
wait "$holder_pid"
assert "migration waits for an in-flight mutex writer" grep -q '"type":"during_lock"' "$canonical/.fno/events.jsonl"
assert "contended journal becomes the shared symlink" test -L "$contended/.fno/events.jsonl"

if (( fail )); then
  exit 1
fi

echo "PASS test-setup-worktree-events.sh"
