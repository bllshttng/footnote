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

mtime() {
  stat -c %Y "$1" 2>/dev/null || stat -f %m "$1"
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

cursor_migration="$TMP/cursor-migration"
mkdir -p "$cursor_migration/.fno"
cursor_consumed='{"ts":"2026-08-11T00:00:00Z","type":"think_offered","source":"backlog","data":{"node_id":"consumed-offer"}}'
cursor_pending='{"ts":"2026-08-11T00:01:00Z","type":"think_offered","source":"backlog","data":{"node_id":"pending-offer"}}'
printf '%s\n%s\n' "$cursor_consumed" "$cursor_pending" > "$cursor_migration/.fno/events.jsonl"
printf '%s' "$(( ${#cursor_consumed} + 1 ))" > "$cursor_migration/.fno/.think-offer-cursor"
canonical_size_before_cursor=$(wc -c < "$canonical/.fno/events.jsonl" | tr -d ' ')
rm -f "$canonical/.fno/.think-offer-cursor"
CANONICAL="$canonical" WORKTREE="$cursor_migration" bash "$SETUP" >/dev/null 2>&1
mapped_cursor=$(cat "$canonical/.fno/.think-offer-cursor")
assert "migration maps the consumed local offer after the canonical prefix" test "$mapped_cursor" -eq "$((canonical_size_before_cursor + ${#cursor_consumed} + 1))"
assert "migration does not replay the consumed local offer" bash -c '! tail -c +"$(( $1 + 1 ))" "$2" | grep -q consumed-offer' _ "$mapped_cursor" "$canonical/.fno/events.jsonl"
assert "migration leaves the pending local offer after the shared cursor" bash -c 'tail -c +"$(( $1 + 1 ))" "$2" | grep -q pending-offer' _ "$mapped_cursor" "$canonical/.fno/events.jsonl"
assert "migrated worktree shares the mapped offer cursor" test -L "$cursor_migration/.fno/.think-offer-cursor"

pending_canonical="$TMP/pending-canonical"
mkdir -p "$pending_canonical/.fno"
printf '%s\n' '{"type":"must_wait_for_canonical_offer"}' > "$pending_canonical/.fno/events.jsonl"
printf '%s' 0 > "$pending_canonical/.fno/.think-offer-cursor"
canonical_size_with_pending=$(wc -c < "$canonical/.fno/events.jsonl" | tr -d ' ')
CANONICAL="$canonical" WORKTREE="$pending_canonical" bash "$SETUP" >/dev/null 2>&1
assert "migration refuses to cross a pending canonical offer" test ! -L "$pending_canonical/.fno/events.jsonl"
assert "refused migration preserves the local offer cursor boundary" test -f "$pending_canonical/.fno/.think-offer-cursor"
printf '%s' "$canonical_size_with_pending" > "$canonical/.fno/.think-offer-cursor"

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

malformed_gate="$TMP/malformed-gate"
mkdir -p "$malformed_gate/.fno"
printf '%s\n' '{"type":"review_attestation","data":{"reviewer":[],"head_sha":"abc","verdict":"fail"}}' >> "$canonical/.fno/events.jsonl"
printf '%s\n' '{"type":"malformed_gate_survivor"}' > "$malformed_gate/.fno/events.jsonl"
CANONICAL="$canonical" WORKTREE="$malformed_gate" bash "$SETUP" >/dev/null 2>&1
assert "malformed gate keys do not abort migration" grep -q 'malformed_gate_survivor' "$canonical/.fno/events.jsonl"

invalid_utf8="$TMP/invalid-utf8"
mkdir -p "$invalid_utf8/.fno"
printf '{"type":"note","data":{"text":"caf\xc3"}}\n' > "$invalid_utf8/.fno/events.jsonl"
CANONICAL="$canonical" WORKTREE="$invalid_utf8" bash "$SETUP" >/dev/null 2>&1
assert "migration preserves malformed UTF-8 rows" python3 -c 'import sys; assert b"caf\xc3" in open(sys.argv[1], "rb").read()' "$canonical/.fno/events.jsonl"

prelink_pending="$TMP/prelink-pending"
mkdir -p "$prelink_pending/.fno"
printf '%s\n' '{"type":"prelink_pending_row"}' > "$prelink_pending/.fno/events.jsonl.pre-share.pending.crash"
CANONICAL="$canonical" WORKTREE="$prelink_pending" bash "$SETUP" >/dev/null 2>&1
assert "pre-link interruption installs the shared symlink" test -L "$prelink_pending/.fno/events.jsonl"
assert "pre-link interruption recovers pending rows" test "$(grep -c 'prelink_pending_row' "$canonical/.fno/events.jsonl" 2>/dev/null || true)" -eq 1
shopt -s nullglob
pending_prelink=("$prelink_pending/.fno/events.jsonl.pre-share.pending."*)
shopt -u nullglob
assert "pre-link interruption clears the pending name" test "${#pending_prelink[@]}" -eq 0

slow_filter="$TMP/slow-filter.py"
cat > "$slow_filter" <<'PY'
import os
import sys
import time

time.sleep(2.5)
os.execv(sys.executable, [sys.executable, os.environ["REAL_EVENTS_MIGRATION_FILTER"], *sys.argv[1:]])
PY
lease_renewal="$TMP/lease-renewal"
mkdir -p "$lease_renewal/.fno"
printf '%s\n' '{"type":"lease_renewal_row"}' > "$lease_renewal/.fno/events.jsonl"
CANONICAL="$canonical" WORKTREE="$lease_renewal" \
  EVENTS_MIGRATION_FILTER="$slow_filter" \
  REAL_EVENTS_MIGRATION_FILTER="$REPO_ROOT/scripts/setup/filter-event-migration.py" \
  EVENTS_MIGRATION_RENEW_SECONDS=0.2 \
  bash "$SETUP" >/dev/null 2>&1 &
lease_setup=$!
for _ in $(seq 1 100); do
  [[ -d "$canonical/.fno/events.jsonl.lock.d" && -d "$lease_renewal/.fno/events.jsonl.lock.d" ]] && break
  sleep 0.05
done
lease_mtime_before=$(mtime "$canonical/.fno/events.jsonl.lock.d")
lease_mtime_after="$lease_mtime_before"
for _ in $(seq 1 40); do
  sleep 0.1
  lease_mtime_after=$(mtime "$canonical/.fno/events.jsonl.lock.d" 2>/dev/null || echo "$lease_mtime_after")
  (( lease_mtime_after > lease_mtime_before )) && break
done
assert "long migration renews held mutex leases" test "$lease_mtime_after" -gt "$lease_mtime_before"
wait "$lease_setup"
assert "lease-renewed migration completes" test -L "$lease_renewal/.fno/events.jsonl"

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

partial_append="$TMP/partial-append"
mkdir -p "$partial_append/.fno"
printf '%s\n' '{"type":"partial_append_row"}' > "$partial_append/.fno/events.jsonl"
canonical_size_before_partial=$(wc -c < "$canonical/.fno/events.jsonl" | tr -d ' ')
cat > "$TMP/partial-cat-env" <<'STUB'
cat() {
  case "${1:-}" in
    *.migration.*)
      command head -c 7 "$1"
      return 1
      ;;
  esac
  command /bin/cat "$@"
}
export -f cat
STUB
CANONICAL="$canonical" WORKTREE="$partial_append" BASH_ENV="$TMP/partial-cat-env" bash "$SETUP" >/dev/null 2>&1
assert "partial append failure leaves canonical journal byte-identical" test "$(wc -c < "$canonical/.fno/events.jsonl" | tr -d ' ')" -eq "$canonical_size_before_partial"
CANONICAL="$canonical" WORKTREE="$partial_append" bash "$SETUP" >/dev/null 2>&1
assert "partial append retry lands the row exactly once" test "$(grep -c 'partial_append_row' "$canonical/.fno/events.jsonl" 2>/dev/null || true)" -eq 1

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

failed_recovery="$TMP/failed-recovery"
mkdir -p "$failed_recovery/.fno"
printf '%s\n' '{"type":"failed_recovery_local_offer"}' > "$failed_recovery/.fno/events.jsonl.pre-share.pending.crash"
ln -s "$canonical/.fno/events.jsonl" "$failed_recovery/.fno/events.jsonl"
printf '%s' 7 > "$failed_recovery/.fno/.think-offer-cursor"
canonical_cursor_before_failure=$(wc -c < "$canonical/.fno/events.jsonl" | tr -d ' ')
printf '%s' "$canonical_cursor_before_failure" > "$canonical/.fno/.think-offer-cursor"
printf '%s\n' '{"ts":"2026-08-11T12:00:00Z","type":"think_offered","source":"backlog","data":{"node_id":"canonical-pending"}}' >> "$canonical/.fno/events.jsonl"
CANONICAL="$canonical" WORKTREE="$failed_recovery" bash "$SETUP" >/dev/null 2>&1
assert "failed pending recovery preserves the local cursor file" test ! -L "$failed_recovery/.fno/.think-offer-cursor"
assert "failed pending recovery preserves the local cursor offset" test "$(cat "$failed_recovery/.fno/.think-offer-cursor")" -eq 7
printf '%s' "$(wc -c < "$canonical/.fno/events.jsonl" | tr -d ' ')" > "$canonical/.fno/.think-offer-cursor"

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
