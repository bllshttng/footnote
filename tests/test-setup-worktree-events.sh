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
assert "fresh worktree shares the offer cursor" test -L "$fresh/.fno/.think-offer-cursor"
assert "offer cursor initializes at canonical EOF" test "$(cat "$canonical/.fno/.think-offer-cursor")" -eq "$(wc -c < "$canonical/.fno/events.jsonl" | tr -d ' ')"
resolved_shell_path=$(
  EVENTS_FILE="$fresh/.fno/events.jsonl"
  # shellcheck disable=SC1090
  source "$REPO_ROOT/scripts/lib/events.sh"
  printf '%s' "$EVENTS_FILE"
)
assert "shell writers resolve the shared target path" test "$resolved_shell_path" -ef "$canonical/.fno/events.jsonl"
looped="$TMP/looped-events.jsonl"
ln -s "$looped" "$looped"
if (EVENTS_FILE="$looped" source "$REPO_ROOT/scripts/lib/events.sh") 2>/dev/null; then
  echo "FAIL: looping events symlink was accepted"
  fail=1
else
  echo "PASS: looping events symlink fails closed"
fi

existing="$TMP/existing"
mkdir -p "$existing/.fno"
printf '%s' '{"type":"worktree_before"}' > "$existing/.fno/events.jsonl"
printf '%s' '999999' > "$existing/.fno/.think-offer-cursor"

CANONICAL="$canonical" WORKTREE="$existing" bash "$SETUP" >/dev/null 2>&1
assert "existing real journal is replaced by a symlink" test -L "$existing/.fno/events.jsonl"
assert "migrated symlink targets the canonical journal" test "$canonical/.fno/events.jsonl" -ef "$existing/.fno/events.jsonl"
assert "stale worktree offer cursor is replaced by the shared cursor" test -L "$existing/.fno/.think-offer-cursor"
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

concurrent="$TMP/concurrent"
mkdir -p "$concurrent/.fno"
printf '%s\n' '{"type":"concurrent_row"}' > "$concurrent/.fno/events.jsonl"
CANONICAL="$canonical" WORKTREE="$concurrent" bash "$SETUP" >/dev/null 2>&1 &
setup_one=$!
CANONICAL="$canonical" WORKTREE="$concurrent" bash "$SETUP" >/dev/null 2>&1 &
setup_two=$!
wait "$setup_one"
wait "$setup_two"
assert "concurrent setup converges on one shared symlink" test -L "$concurrent/.fno/events.jsonl"
assert "concurrent setup migrates local rows once" test "$(grep -c 'concurrent_row' "$canonical/.fno/events.jsonl" 2>/dev/null || true)" -eq 1

ordered="$TMP/ordered"
mkdir -p "$ordered/.fno"
printf '%s\n' '{"ts":"2026-08-11T10:00:00Z","type":"review_attestation","source":"target","data":{"reviewer":"code-review","head_sha":"abc","verdict":"fail","session_id":"canonical"}}' >> "$canonical/.fno/events.jsonl"
printf '%s\n' '{"ts":"2026-08-11T09:00:00Z","type":"review_attestation","source":"target","data":{"reviewer":"/code-review","head_sha":"abc","verdict":"pass","session_id":"local"}}' > "$ordered/.fno/events.jsonl"
CANONICAL="$canonical" WORKTREE="$ordered" bash "$SETUP" >/dev/null 2>&1
last_verdict=$(jq -r 'select(.type == "review_attestation" and .data.reviewer == "code-review" and .data.head_sha == "abc") | .data.verdict' "$canonical/.fno/events.jsonl" | tail -1)
assert "migration cannot restore older passing gate evidence" test "$last_verdict" = fail

invalid_utf8="$TMP/invalid-utf8"
mkdir -p "$invalid_utf8/.fno"
printf '{"type":"note","data":{"text":"caf\xc3"}}\n' > "$invalid_utf8/.fno/events.jsonl"
CANONICAL="$canonical" WORKTREE="$invalid_utf8" bash "$SETUP" >/dev/null 2>&1
assert "migration preserves malformed UTF-8 rows" python3 -c 'import sys; assert b"caf\xc3" in open(sys.argv[1], "rb").read()' "$canonical/.fno/events.jsonl"

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

shell_active="$TMP/shell-active"
sleep 30 &
shell_writer_pid=$!
mkdir -p "$shell_active/.fno/events.jsonl.shell-writers.d/${shell_writer_pid}.1"
printf '%s\n' '{"type":"before_shell"}' > "$shell_active/.fno/events.jsonl"
CANONICAL="$canonical" WORKTREE="$shell_active" bash "$SETUP" >/dev/null 2>&1 &
setup_pid=$!
for _ in {1..100}; do
  [[ -d "$shell_active/.fno/events.jsonl.gc.d" ]] && break
  sleep 0.01
done
assert "migration publishes its GC marker" test -d "$shell_active/.fno/events.jsonl.gc.d"
sleep 0.2
assert "migration does not switch an active shell writer" test ! -L "$shell_active/.fno/events.jsonl"
kill "$shell_writer_pid" 2>/dev/null || true
wait "$shell_writer_pid" 2>/dev/null || true
rmdir "$shell_active/.fno/events.jsonl.shell-writers.d/${shell_writer_pid}.1"
rmdir "$shell_active/.fno/events.jsonl.shell-writers.d"
wait "$setup_pid"
assert "shell-rendezvous journal becomes the shared symlink" test -L "$shell_active/.fno/events.jsonl"

dead_shell="$TMP/dead-shell"
mkdir -p "$dead_shell/.fno/events.jsonl.shell-writers.d/999999.1"
printf '%s\n' '{"type":"before_dead_shell"}' > "$dead_shell/.fno/events.jsonl"
CANONICAL="$canonical" WORKTREE="$dead_shell" bash "$SETUP" >/dev/null 2>&1
assert "migration reaps a dead shell-writer token" test -L "$dead_shell/.fno/events.jsonl"

failed_once="$TMP/failed-once"
mkdir -p "$failed_once/.fno"
printf '%s\n' '{"type":"failed_once_row"}' > "$failed_once/.fno/events.jsonl"
cat > "$TMP/fail-env" <<'STUB'
mv() {
  if [[ "$1" == *"/.fno/events.jsonl" ]]; then
    return 1
  fi
  command /bin/mv "$@"
}
export -f mv
STUB
CANONICAL="$canonical" WORKTREE="$failed_once" BASH_ENV="$TMP/fail-env" bash "$SETUP" >/dev/null 2>&1
assert "failed link leaves canonical journal unmodified" test "$(grep -c 'failed_once_row' "$canonical/.fno/events.jsonl" 2>/dev/null || true)" -eq 0
CANONICAL="$canonical" WORKTREE="$failed_once" bash "$SETUP" >/dev/null 2>&1
assert "migration retry appends a failed row exactly once" test "$(grep -c 'failed_once_row' "$canonical/.fno/events.jsonl" 2>/dev/null || true)" -eq 1
assert "migration retry installs the shared symlink" test -L "$failed_once/.fno/events.jsonl"

interrupted="$TMP/interrupted"
mkdir -p "$interrupted/.fno"
printf '%s\n' '{"type":"interrupted_row"}' > "$interrupted/.fno/events.jsonl.pre-share.pending.crash"
ln -s "$canonical/.fno/events.jsonl" "$interrupted/.fno/events.jsonl"
CANONICAL="$canonical" WORKTREE="$interrupted" bash "$SETUP" >/dev/null 2>&1
assert "interrupted symlink migration recovers its pending rows" test "$(grep -c 'interrupted_row' "$canonical/.fno/events.jsonl" 2>/dev/null || true)" -eq 1
assert "interrupted migration clears its pending backup" test ! -e "$interrupted/.fno/events.jsonl.pre-share.pending.crash"
CANONICAL="$canonical" WORKTREE="$interrupted" bash "$SETUP" >/dev/null 2>&1
assert "interrupted migration recovery is idempotent" test "$(grep -c 'interrupted_row' "$canonical/.fno/events.jsonl" 2>/dev/null || true)" -eq 1

post_append="$TMP/post-append"
mkdir -p "$post_append/.fno"
printf '%s\n' '{"type":"post_append_row"}' >> "$canonical/.fno/events.jsonl"
printf '%s\n' '{"type":"post_append_row"}' > "$post_append/.fno/events.jsonl.pre-share.pending.crash"
ln -s "$canonical/.fno/events.jsonl" "$post_append/.fno/events.jsonl"
CANONICAL="$canonical" WORKTREE="$post_append" bash "$SETUP" >/dev/null 2>&1
assert "post-append recovery does not duplicate landed rows" test "$(grep -c 'post_append_row' "$canonical/.fno/events.jsonl" 2>/dev/null || true)" -eq 1

stale_marker="$TMP/stale-marker"
mkdir -p "$stale_marker/.fno/events.jsonl.gc.d"
printf '%s\n' '{"type":"stale_marker_row"}' > "$stale_marker/.fno/events.jsonl"
printf '%s' 'dead:999999:marker' > "$stale_marker/.fno/events.jsonl.gc.d/owner"
touch -t 202001010000 "$stale_marker/.fno/events.jsonl.gc.d"
CANONICAL="$canonical" WORKTREE="$stale_marker" bash "$SETUP" >/dev/null 2>&1
assert "migration reaps an abandoned GC marker" test -L "$stale_marker/.fno/events.jsonl"

if (( fail )); then
  exit 1
fi

echo "PASS test-setup-worktree-events.sh"
