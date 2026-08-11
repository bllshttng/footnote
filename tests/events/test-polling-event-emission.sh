#!/usr/bin/env bash
# tests/events/test-polling-event-emission.sh
#
# Tests for emit_polling_external_review (Phase 1 task 1.1 of
# loop-correctness-sweep, plan ab-83be25ea). One assertion per case;
# sets fail=1 on any failure so every case runs before exit.
#
# Run: bash tests/events/test-polling-event-emission.sh

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
EVENTS_LIB="$REPO_ROOT/scripts/lib/events.sh"
VALIDATOR="$REPO_ROOT/scripts/lib/events-validate.sh"

if [[ ! -r "$EVENTS_LIB" ]]; then
    echo "FAIL: events lib not found at $EVENTS_LIB"
    exit 1
fi

# shellcheck disable=SC1090
source "$EVENTS_LIB"
if [[ -r "$VALIDATOR" ]]; then
    # shellcheck disable=SC1090
    source "$VALIDATOR"
fi

fail=0

assert_eq() {
    local label="$1" expected="$2" actual="$3"
    if [[ "$expected" != "$actual" ]]; then
        echo "FAIL $label: expected=$expected actual=$actual"
        fail=1
    fi
}

assert_contains() {
    local label="$1" haystack="$2" needle="$3"
    if [[ "$haystack" != *"$needle"* ]]; then
        echo "FAIL $label: missing '$needle' in: $haystack"
        fail=1
    fi
}

assert_not_contains() {
    local label="$1" haystack="$2" needle="$3"
    if [[ "$haystack" == *"$needle"* ]]; then
        echo "FAIL $label: unexpected '$needle' in: $haystack"
        fail=1
    fi
}

WORK=$(mktemp -d -t polling-emit-XXXXXX)
trap 'rm -rf "$WORK"' EXIT
export EVENTS_FILE="$WORK/events.jsonl"

# AC1-HP: Happy Path - all required fields, optional next_check_at
rm -f "$EVENTS_FILE"
out=$(emit_polling_external_review \
    pr_number=204 \
    reviewer_bot='gemini-code-assist[bot]' \
    wait_kind=cron \
    next_check_at=2026-05-08T16:00:00Z \
    session_id=s-abc 2>&1)
rc=$?
assert_eq "AC1-HP rc" 0 $rc
assert_eq "AC1-HP stderr empty" "" "$out"
[[ -f "$EVENTS_FILE" ]] || { echo "FAIL AC1-HP: events file not created"; fail=1; }
line=$(tail -1 "$EVENTS_FILE" 2>/dev/null)
assert_contains "AC1-HP type" "$line" '"type":"polling_external_review"'
assert_contains "AC1-HP pr_number" "$line" '"pr_number":204'
assert_contains "AC1-HP reviewer_bot" "$line" '"reviewer_bot":"gemini-code-assist[bot]"'
assert_contains "AC1-HP wait_kind" "$line" '"wait_kind":"cron"'
assert_contains "AC1-HP next_check_at" "$line" '"next_check_at":"2026-05-08T16:00:00Z"'
assert_contains "AC1-HP session_id" "$line" '"session_id":"s-abc"'
assert_contains "AC1-HP source target default" "$line" '"source":"target"'

# AC1-HP-2: inline wait_kind, no next_check_at, custom source via env
rm -f "$EVENTS_FILE"
out=$(EMIT_SOURCE_ID=hook emit_polling_external_review \
    pr_number=42 \
    reviewer_bot=somebot \
    wait_kind=inline \
    session_id=s-xyz 2>&1)
rc=$?
assert_eq "AC1-HP-2 rc" 0 $rc
line=$(tail -1 "$EVENTS_FILE" 2>/dev/null)
assert_contains "AC1-HP-2 wait_kind inline" "$line" '"wait_kind":"inline"'
assert_contains "AC1-HP-2 source override" "$line" '"source":"hook"'
assert_not_contains "AC1-HP-2 omits next_check_at" "$line" '"next_check_at"'

# AC2-ERR: missing pr_number
rm -f "$EVENTS_FILE"
out=$(emit_polling_external_review reviewer_bot=b wait_kind=cron session_id=s 2>&1)
rc=$?
assert_eq "AC2-ERR missing-pr rc" 1 $rc
assert_contains "AC2-ERR missing-pr msg" "$out" "missing pr_number"
[[ ! -s "$EVENTS_FILE" ]] || { echo "FAIL AC2-ERR missing-pr: events file written"; fail=1; }

# AC2-ERR: invalid wait_kind
rm -f "$EVENTS_FILE"
out=$(emit_polling_external_review pr_number=1 reviewer_bot=b wait_kind=bogus session_id=s 2>&1)
rc=$?
assert_eq "AC2-ERR bad-wait_kind rc" 1 $rc
assert_contains "AC2-ERR bad-wait_kind msg" "$out" "wait_kind"
[[ ! -s "$EVENTS_FILE" ]] || { echo "FAIL AC2-ERR bad-wait_kind: events file written"; fail=1; }

# AC2-ERR: missing reviewer_bot
rm -f "$EVENTS_FILE"
out=$(emit_polling_external_review pr_number=1 wait_kind=cron session_id=s 2>&1)
rc=$?
assert_eq "AC2-ERR missing-reviewer rc" 1 $rc
assert_contains "AC2-ERR missing-reviewer msg" "$out" "reviewer_bot"

# AC2-ERR: missing session_id
rm -f "$EVENTS_FILE"
out=$(emit_polling_external_review pr_number=1 reviewer_bot=b wait_kind=cron 2>&1)
rc=$?
assert_eq "AC2-ERR missing-sid rc" 1 $rc
assert_contains "AC2-ERR missing-sid msg" "$out" "session_id"

# AC2-ERR: unknown key surfaces
rm -f "$EVENTS_FILE"
out=$(emit_polling_external_review pr_number=1 reviewer_bot=b wait_kind=cron session_id=s extra=junk 2>&1)
rc=$?
assert_eq "AC2-ERR unknown-key rc" 1 $rc
assert_contains "AC2-ERR unknown-key msg" "$out" "unknown key"

# AC4-EDGE: concurrent emissions both land
rm -f "$EVENTS_FILE"
(
    emit_polling_external_review pr_number=1 reviewer_bot=a wait_kind=cron session_id=s &
    emit_polling_external_review pr_number=2 reviewer_bot=b wait_kind=inline session_id=s &
    wait
)
count=$(grep -c '"type":"polling_external_review"' "$EVENTS_FILE" 2>/dev/null || echo 0)
assert_eq "AC4-EDGE concurrent count" 2 $count
# Each line must be a single valid JSON object (no interleaving)
while IFS= read -r line; do
    if ! jq -e . <<<"$line" >/dev/null 2>&1; then
        echo "FAIL AC4-EDGE: corrupted line: $line"
        fail=1
    fi
done < "$EVENTS_FILE"

# Unlocked shell writers must keep every append below the atomic line bound.
large_value=$(printf '%05000d' 0)

rm -f "$EVENTS_FILE"
emit_event target size_probe "$(jq -nc --arg value "$large_value" '{value: $value}')" 2>/dev/null
[[ ! -s "$EVENTS_FILE" ]] || { echo "FAIL size cap: emit_event appended an oversized line"; fail=1; }

rm -f "$EVENTS_FILE"
emit_event_raw size_probe "$(jq -nc --arg value "$large_value" '{value: $value}')" 2>/dev/null
[[ ! -s "$EVENTS_FILE" ]] || { echo "FAIL size cap: emit_event_raw appended an oversized line"; fail=1; }

rm -f "$EVENTS_FILE"
out=$(emit_polling_external_review \
    pr_number=1 \
    reviewer_bot="$large_value" \
    wait_kind=inline \
    session_id=s 2>&1)
rc=$?
assert_eq "size cap polling rc" 2 "$rc"
assert_contains "size cap polling message" "$out" "exceeds"
[[ ! -s "$EVENTS_FILE" ]] || { echo "FAIL size cap: polling emitter appended an oversized line"; fail=1; }

# Default paths resolve from the worktree root even when sourced below it.
repo_root="$WORK/repo"
mkdir -p "$repo_root/nested/source"
git -C "$repo_root" init -q
(
    unset EVENTS_FILE
    cd "$repo_root/nested/source" || exit 1
    # shellcheck disable=SC1090
    source "$EVENTS_LIB"
    emit_event target root_probe '{}'
)
[[ -s "$repo_root/.fno/events.jsonl" ]] || { echo "FAIL repo root: shell event did not land at root"; fail=1; }
[[ ! -e "$repo_root/nested/source/.fno" ]] || { echo "FAIL repo root: shell event created a nested .fno"; fail=1; }

# A collector marker pauses the unlocked shell append until replacement is done.
rm -f "$EVENTS_FILE"
mkdir "${EVENTS_FILE}.gc.d"
(
    sleep 0.1
    rmdir "${EVENTS_FILE}.gc.d"
) &
gc_release_pid=$!
emit_event target gc_barrier_probe '{}'
wait "$gc_release_pid"
line=$(tail -1 "$EVENTS_FILE" 2>/dev/null)
assert_contains "GC barrier append" "$line" '"type":"gc_barrier_probe"'

# Hook emitters must return well inside the harness's 30-second hook deadline
# when maintenance stays live; losing this best-effort row must not suppress a
# stop-hook decision that follows it.
rm -f "$EVENTS_FILE"
mkdir "${EVENTS_FILE}.gc.d"
started=$(python3 -c 'import time; print(time.monotonic())')
EVENTS_GC_WAIT_ATTEMPTS=2 emit_event target bounded_gc_wait_probe '{}'
finished=$(python3 -c 'import time; print(time.monotonic())')
rmdir "${EVENTS_FILE}.gc.d"
if ! python3 - "$started" "$finished" <<'PY'
import sys

raise SystemExit(0 if float(sys.argv[2]) - float(sys.argv[1]) < 2 else 1)
PY
then
    echo "FAIL GC barrier: shell emitter exceeded its bounded hook wait"
    fail=1
fi
[[ ! -s "$EVENTS_FILE" ]] || { echo "FAIL GC barrier: bounded-out emitter appended during maintenance"; fail=1; }

# A killed collector must not leave shell writers blocked forever.
rm -f "$EVENTS_FILE"
mkdir "${EVENTS_FILE}.gc.d"
printf '%s' 'dead:999999:marker' > "${EVENTS_FILE}.gc.d/owner"
touch -t 202001010000 "${EVENTS_FILE}.gc.d"
emit_event target stale_gc_probe '{}'
line=$(tail -1 "$EVENTS_FILE" 2>/dev/null)
assert_contains "stale GC marker recovery" "$line" '"type":"stale_gc_probe"'
[[ ! -d "${EVENTS_FILE}.gc.d" ]] || { echo "FAIL stale GC marker was not reaped"; fail=1; }

# A stale marker can be replaced by a fresh holder between the age read and
# steal attempt. The stealer must bind the old owner before observing age and
# leave the fresh holder intact.
identity_lock="$WORK/stale-identity.gc.d"
mkdir "$identity_lock"
printf '%s' old-holder > "$identity_lock/owner"
touch -t 202001010000 "$identity_lock"
identity_result=$(
    (
        # shellcheck disable=SC1090
        source "$EVENTS_LIB"
        old_modified=$(command stat -c %Y "$identity_lock" 2>/dev/null || command stat -f %m "$identity_lock")
        stat() {
            command rm -f "$identity_lock/owner"
            rmdir "$identity_lock"
            mkdir "$identity_lock"
            printf '%s' fresh-holder > "$identity_lock/owner"
            printf '%s\n' "$old_modified"
        }
        if _steal_stale_event_dir "$identity_lock"; then
            printf '%s\n' STOLEN
        else
            printf '%s\n' PRESERVED
        fi
        printf 'OWNER=%s\n' "$(cat "$identity_lock/owner" 2>/dev/null || true)"
    )
)
assert_contains "stale identity replacement preserved" "$identity_result" PRESERVED
assert_contains "fresh stale-marker owner preserved" "$identity_result" OWNER=fresh-holder

# A long holder can renew after a stealer observes the old age. The owner token
# is unchanged, so the post-rename mtime check is what preserves the live lease.
renewed_lock="$WORK/stale-renewed.gc.d"
mkdir "$renewed_lock"
printf '%s' live-holder > "$renewed_lock/owner"
touch -t 202001010000 "$renewed_lock"
renewed_result=$(
    (
        # shellcheck disable=SC1090
        source "$EVENTS_LIB"
        mv() {
            command touch "$1"
            command mv "$@"
        }
        if _steal_stale_event_dir "$renewed_lock"; then
            printf '%s\n' STOLEN
        else
            printf '%s\n' PRESERVED
        fi
        printf 'OWNER=%s\n' "$(cat "$renewed_lock/owner" 2>/dev/null || true)"
    )
)
assert_contains "same-owner lease renewal preserved" "$renewed_result" PRESERVED
assert_contains "renewed stale-marker owner preserved" "$renewed_result" OWNER=live-holder

# A third holder can acquire the canonical path after the mismatched holder was
# reaped but before it is restored. A directory-targeting mv must not nest the
# reaped lock inside that third holder and leave the visible lock non-empty.
restore_race_lock="$WORK/stale-restore-race.gc.d"
mkdir "$restore_race_lock"
printf '%s' old-holder > "$restore_race_lock/owner"
touch -t 202001010000 "$restore_race_lock"
restore_race_result=$(
    (
        # shellcheck disable=SC1090
        source "$EVENTS_LIB"
        old_modified=$(command stat -c %Y "$restore_race_lock" 2>/dev/null || command stat -f %m "$restore_race_lock")
        stat() {
            printf '%s\n' "$old_modified"
        }
        restore_move_seen="$WORK/stale-restore-move-seen"
        mv() {
            if mkdir "$restore_move_seen" 2>/dev/null; then
                command rm -f "$restore_race_lock/owner"
                rmdir "$restore_race_lock"
                mkdir "$restore_race_lock"
                printf '%s' fresh-holder > "$restore_race_lock/owner"
                command mv "$@"
                local rc=$?
                mkdir "$restore_race_lock"
                printf '%s' newest-holder > "$restore_race_lock/owner"
                return "$rc"
            fi
            command mv "$@"
        }
        if _steal_stale_event_dir "$restore_race_lock"; then
            printf '%s\n' STOLEN
        else
            printf '%s\n' PRESERVED
        fi
        printf 'OWNER=%s\n' "$(cat "$restore_race_lock/owner" 2>/dev/null || true)"
        if find "$restore_race_lock" -mindepth 1 -maxdepth 1 -type d -name '*.reap.*' -print -quit | grep -q .; then
            printf '%s\n' NESTED
        else
            printf '%s\n' FLAT
        fi
    )
)
assert_contains "stale restore race preserves newest holder" "$restore_race_result" OWNER=newest-holder
assert_contains "stale restore race never nests reaped lock" "$restore_race_result" FLAT

# Explicit paths passed by hook fallbacks must share the canonical journal's
# GC marker and writer rendezvous when the worktree leaf is a symlink.
canonical_events="$WORK/canonical/events.jsonl"
linked_events="$WORK/linked/events.jsonl"
mkdir -p "$(dirname "$canonical_events")" "$(dirname "$linked_events")"
ln -s "$canonical_events" "$linked_events"
mkdir "${canonical_events}.gc.d"
(
    sleep 0.2
    rmdir "${canonical_events}.gc.d"
) &
linked_gc_release_pid=$!
(
    _append_bounded_event explicit_symlink_probe '{"type":"explicit_symlink_probe"}' "$linked_events"
    touch "$WORK/explicit-symlink-done"
) &
linked_append_pid=$!
sleep 0.05
[[ ! -e "$WORK/explicit-symlink-done" ]] || { echo "FAIL explicit symlink: append bypassed canonical GC marker"; fail=1; }
wait "$linked_gc_release_pid"
wait "$linked_append_pid"
line=$(tail -1 "$canonical_events" 2>/dev/null)
assert_contains "explicit symlink GC barrier append" "$line" '"type":"explicit_symlink_probe"'

# Setup can install the shared-journal symlink while a shell writer waits for
# maintenance. The writer must discard its local rendezvous token and register
# against the canonical journal before appending through the new link.
handoff_local="$WORK/handoff-local/events.jsonl"
handoff_canonical="$WORK/handoff-canonical/events.jsonl"
mkdir -p "$(dirname "$handoff_local")" "$(dirname "$handoff_canonical")"
: > "$handoff_local"
: > "$handoff_canonical"
handoff_result=$(
    (
        # shellcheck disable=SC1090
        source "$EVENTS_LIB"
        _wait_for_event_gc() {
            if [[ ! -L "$handoff_local" ]]; then
                mv "$handoff_local" "${handoff_local}.pending"
                ln -s "$handoff_canonical" "$handoff_local"
            fi
            return 0
        }
        _end_shell_event_append() {
            local token="${1:?writer token required}"
            printf 'TOKEN=%s\n' "$token"
            command -p rm -f "$token/owner" 2>/dev/null || true
            rmdir "$token" 2>/dev/null || true
            rmdir "$(dirname "$token")" 2>/dev/null || true
        }
        _append_bounded_event handoff_probe '{"type":"handoff_probe"}' "$handoff_local"
    )
)
rc=$?
assert_eq "setup handoff append rc" 0 "$rc"
handoff_canonical_resolved="$(cd "$(dirname "$handoff_canonical")" && pwd -P)/$(basename "$handoff_canonical")"
assert_contains "setup handoff canonical token" "$handoff_result" "TOKEN=${handoff_canonical_resolved}.shell-writers.d/"
line=$(tail -1 "$handoff_canonical" 2>/dev/null)
assert_contains "setup handoff append" "$line" '"type":"handoff_probe"'

# A collector can publish its marker after token registration but before the
# writer's second marker check. Retrying must remove the token's owner file too,
# or collector and writer wait on each other until both time out.
registration_events="$WORK/registration-race/events.jsonl"
mkdir -p "$(dirname "$registration_events")"
: > "$registration_events"
registration_result=$(
    (
        # shellcheck disable=SC1090
        source "$EVENTS_LIB"
        identity_once="$WORK/registration-marker-created"
        _event_process_identity() {
            if [[ ! -e "$identity_once" ]]; then
                : > "$identity_once"
                mkdir "${registration_events}.gc.d"
            fi
            printf '%s' test-process
        }
        _wait_for_event_gc() {
            local marker="${1}.gc.d"
            if [[ -d "$marker" ]]; then
                local active="${1}.shell-writers.d"
                shopt -s nullglob
                local entries=("$active"/*)
                shopt -u nullglob
                [[ -z "${entries[0]:-}" ]] || return 1
                rmdir "$marker"
            fi
            return 0
        }
        token=$(_begin_shell_event_append "$registration_events" "${BASHPID:-$$}") || exit 1
        printf 'TOKEN=%s\n' "$token"
        _end_shell_event_append "$token"
    )
)
rc=$?
assert_eq "registration marker race rc" 0 "$rc"
assert_contains "registration marker race retries" "$registration_result" "TOKEN=${registration_events}.shell-writers.d/"

# AC-VALIDATOR: validator accepts canonical envelope (when validator is loadable)
if declare -F validate_event >/dev/null 2>&1; then
    canonical='{"ts":"2026-05-07T09:30:42Z","type":"polling_external_review","source":"target","data":{"pr_number":204,"reviewer_bot":"gemini-code-assist[bot]","wait_kind":"cron","session_id":"s","next_check_at":"2026-05-08T16:00:00Z"}}'
    if ! validate_event polling_external_review "$canonical" 2>&1; then
        echo "FAIL AC-VALIDATOR: validator rejected canonical event"
        fail=1
    fi
    # Reject missing wait_kind
    bad='{"ts":"2026-05-07T09:30:42Z","type":"polling_external_review","source":"target","data":{"pr_number":1,"reviewer_bot":"b","session_id":"s"}}'
    out=$(validate_event polling_external_review "$bad" 2>&1)
    rc=$?
    assert_eq "AC-VALIDATOR missing-wait_kind rc" 1 $rc
fi

if (( fail == 0 )); then
    echo "PASS test-polling-event-emission.sh"
    exit 0
else
    echo "FAIL test-polling-event-emission.sh"
    exit 1
fi
