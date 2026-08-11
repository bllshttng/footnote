#!/usr/bin/env bash
# setup-worktree.sh - link gitignored shared files from the canonical project
# into a worktree. Idempotent and never destructive.
#
# Usage:
#   bash scripts/setup/setup-worktree.sh                           # auto-detect canonical
#   CANONICAL=/path/to/canonical bash scripts/setup/setup-worktree.sh
#
# Conductor calls this via conductor.json's scripts.setup hook with
# CONDUCTOR_ROOT_PATH set to the canonical project. Manual `git worktree
# add` or the fno git-worktrees skill should call this directly.
#
# Safety contract (load-bearing):
#   - Uses `ln -sf` to create or refresh symlinks; never `rm -rf` a target
#   - If a target already exists as a real (non-symlink) file or directory,
#     SKIP it with a stderr warning, except events.jsonl's lock-protected migration
#   - Never deletes an existing symlink either; ln -sf replaces atomically
#   - Each link is independent so a failure on one does not block the rest

set -euo pipefail

# Defensive PATH - some worktrees inherit a stripped PATH from per-directory
# env hooks (direnv, etc.). Prepend the standard system paths so coreutils
# (mkdir, ln, rm, ls) always resolve.
export PATH="/usr/bin:/bin:/usr/sbin:/sbin:${PATH:-}"

# shellcheck source=../lib/events-lock.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib/events-lock.sh"
EVENTS_MIGRATION_FILTER="${EVENTS_MIGRATION_FILTER:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/filter-event-migration.py}"
EVENTS_MIGRATION_RENEW_SECONDS="${EVENTS_MIGRATION_RENEW_SECONDS:-30}"

# Resolve canonical project root (where the shared files live). Priority:
#   1. CANONICAL env var (manual override)
#   2. CONDUCTOR_ROOT_PATH (set by Conductor when invoking via scripts.setup)
#   3. git-common-dir resolution (works from any worktree of the same repo)
#   4. $HOME/code/me/fno (last-ditch fallback for non-git contexts)
CANONICAL="${CANONICAL:-${CONDUCTOR_ROOT_PATH:-}}"
if [[ -z "$CANONICAL" ]]; then
  # In a worktree, git-common-dir points at the main repo's .git directory.
  # Going one level up gets the canonical worktree (the main checkout).
  COMMON_DIR=$(git rev-parse --git-common-dir 2>/dev/null || true)
  if [[ -n "$COMMON_DIR" && -d "$COMMON_DIR" ]]; then
    CANONICAL=$(cd "$COMMON_DIR/.." && pwd)
  else
    CANONICAL="$HOME/code/me/fno"
  fi
fi

if [[ ! -d "$CANONICAL" ]]; then
  echo "setup-worktree: canonical project not found at $CANONICAL" >&2
  exit 1
fi

# Resolve worktree root (where we are linking INTO). Default to cwd.
WORKTREE="${WORKTREE:-$(pwd)}"

# Refuse the whole script when the two roots are one directory, BEFORE the
# mkdir and every link helper. Not a check inside link_artifact/link_file/
# link_dir: when the roots coincide the script is a no-op at best and
# destructive at worst, so the answer is to not run it at all. link_artifact
# `rm -f`s the real file before `ln -sf "$source" "$target"`, which with equal
# paths leaves a symlink pointing at itself - that is how this repo's
# .fno/codemap.md was lost on 2026-07-26.
#
# `-ef` compares device+inode, so a symlinked, relative, or /tmp-vs-/private/tmp
# invocation cannot slip past it the way a string equality test would.
#
# Exit 0, not non-zero: "already canonical, nothing to link" is a successful
# no-op, and `fno target start` treats any non-zero from this script as fatal
# ("refusing to initialize against unverified shared state", target_cli.py).
if [[ "$CANONICAL" -ef "$WORKTREE" ]]; then
  echo "setup-worktree: refusing to symlink canonical -> canonical (no-op): $CANONICAL" >&2
  exit 0
fi

mkdir -p "$WORKTREE/.fno" "$WORKTREE/.claude"

# Link a single file. Skips if target is already a non-symlink real file.
# Reserved for files where local divergence might be user data we cannot lose
# (settings, ledgers, task lists).
link_file() {
  local rel="$1"
  local source="$CANONICAL/$rel"
  local target="$WORKTREE/$rel"

  if [[ ! -e "$source" ]]; then
    echo "setup-worktree: source missing, skipping: $rel" >&2
    return 0
  fi

  if [[ -e "$target" && ! -L "$target" ]]; then
    echo "setup-worktree: refusing to overwrite real file: $target" >&2
    return 0
  fi

  ln -sf "$source" "$target"
}

# Link a regenerable artifact. Replaces existing real files in the worktree
# because the canonical copy is authoritative and the artifact is rebuilt
# on demand (e.g. codemap.md). NEVER call this on user data.
link_artifact() {
  local rel="$1"
  local source="$CANONICAL/$rel"
  local target="$WORKTREE/$rel"

  if [[ ! -e "$source" ]]; then
    echo "setup-worktree: source missing, skipping: $rel" >&2
    return 0
  fi

  # If the target is a real file (not a symlink), replace it. Real dirs are
  # NOT replaced by this helper - that's link_dir's job and it has its own
  # safety check.
  if [[ -e "$target" && ! -L "$target" && ! -d "$target" ]]; then
    rm -f "$target"
  fi

  ln -sf "$source" "$target"
}

# Link a directory by symlinking the dir itself (not its contents).
# Same skip-if-real-dir-exists rule as link_file.
link_dir() {
  local rel="$1"
  local source="$CANONICAL/$rel"
  local target="$WORKTREE/$rel"

  if [[ ! -d "$source" ]]; then
    echo "setup-worktree: source dir missing, skipping: $rel" >&2
    return 0
  fi

  if [[ -e "$target" && ! -L "$target" ]]; then
    if [[ -d "$target" ]] && [[ -n "$(ls -A "$target" 2>/dev/null || true)" ]]; then
      echo "setup-worktree: refusing to overwrite non-empty real dir: $target" >&2
      return 0
    fi
    # Empty real dir - safe to remove and replace with symlink. Uses rmdir
    # which only works on empty dirs (will fail loudly otherwise).
    rmdir "$target" 2>/dev/null || {
      echo "setup-worktree: could not remove existing $target, skipping" >&2
      return 0
    }
  fi

  # -n / --no-dereference: when target already exists as a symlink-to-dir,
  # treat it as the link name (replace it in place) rather than following
  # it and creating a new link INSIDE it. Without -n, a repeat run lands a
  # recursive symlink `target/<basename(target)>` inside the canonical
  # destination, polluting shared state. Both BSD (macOS) and GNU `ln`
  # accept -n. Codex flagged this on PR #320 round 3.
  ln -sfn "$source" "$target"
}

# Acquire the same owner-token mkdir mutex used by the Python and Rust event
# writers, including their age-gated recovery for an abandoned directory.
acquire_events_dir() {
  local lock_dir="$1"
  local token="$2"
  local attempts=0
  while ! mkdir "$lock_dir" 2>/dev/null; do
    renew_events_migration_dirs || return 1
    if _steal_stale_event_dir "$lock_dir"; then
      continue
    fi
    if (( attempts >= 300 )); then
      return 1
    fi
    sleep 0.1
    attempts=$((attempts + 1))
  done
  if ! printf '%s' "$token" > "$lock_dir/owner" 2>/dev/null; then
    rmdir "$lock_dir" 2>/dev/null || true
    return 1
  fi
}

release_events_dir() {
  local lock_dir="$1"
  local token="$2"
  [[ -d "$lock_dir" ]] || return 0
  [[ -r "$lock_dir/owner" ]] || return 0
  [[ "$(< "$lock_dir/owner")" == "$token" ]] || return 0
  rm -f "$lock_dir/owner"
  rmdir "$lock_dir" 2>/dev/null || true
}

renew_events_dir() {
  local lock_dir="$1"
  local token="$2"
  [[ -r "$lock_dir/owner" ]] || return 1
  [[ "$(< "$lock_dir/owner")" == "$token" ]] || return 1
  touch "$lock_dir" 2>/dev/null || return 1
  [[ "$(< "$lock_dir/owner")" == "$token" ]]
}

EVENTS_MIGRATION_TOKEN=""
EVENTS_MIGRATION_DIRS=()
EVENTS_MIGRATION_KEEPALIVE_PID=""
EVENTS_MIGRATION_LEASE_FAILED=""

renew_events_migration_dirs() {
  local lock_dir
  for lock_dir in "${EVENTS_MIGRATION_DIRS[@]}"; do
    renew_events_dir "$lock_dir" "$EVENTS_MIGRATION_TOKEN" || return 1
  done
}

verify_events_migration_leases() {
  [[ -z "$EVENTS_MIGRATION_LEASE_FAILED" || ! -e "$EVENTS_MIGRATION_LEASE_FAILED" ]] \
    && renew_events_migration_dirs
}

start_events_migration_keepalive() {
  EVENTS_MIGRATION_LEASE_FAILED=$(mktemp -t fno-events-migration-lease.XXXXXX) || return 1
  command -p rm -f "$EVENTS_MIGRATION_LEASE_FAILED"
  (
    trap - EXIT INT TERM
    while sleep "$EVENTS_MIGRATION_RENEW_SECONDS"; do
      local lock_dir
      for lock_dir in "${EVENTS_MIGRATION_DIRS[@]}"; do
        if ! renew_events_dir "$lock_dir" "$EVENTS_MIGRATION_TOKEN"; then
          : > "$EVENTS_MIGRATION_LEASE_FAILED"
          exit 1
        fi
      done
    done
  ) &
  EVENTS_MIGRATION_KEEPALIVE_PID=$!
}

stop_events_migration_keepalive() {
  local failed=0
  if [[ -n "$EVENTS_MIGRATION_KEEPALIVE_PID" ]]; then
    kill "$EVENTS_MIGRATION_KEEPALIVE_PID" 2>/dev/null || true
    wait "$EVENTS_MIGRATION_KEEPALIVE_PID" 2>/dev/null || true
  fi
  [[ -n "$EVENTS_MIGRATION_LEASE_FAILED" && -e "$EVENTS_MIGRATION_LEASE_FAILED" ]] && failed=1
  [[ -n "$EVENTS_MIGRATION_LEASE_FAILED" ]] && command -p rm -f "$EVENTS_MIGRATION_LEASE_FAILED"
  EVENTS_MIGRATION_KEEPALIVE_PID=""
  EVENTS_MIGRATION_LEASE_FAILED=""
  return "$failed"
}

cleanup_events_migration() {
  local index
  stop_events_migration_keepalive || true
  for ((index=${#EVENTS_MIGRATION_DIRS[@]} - 1; index >= 0; index--)); do
    release_events_dir "${EVENTS_MIGRATION_DIRS[index]}" "$EVENTS_MIGRATION_TOKEN"
  done
  EVENTS_MIGRATION_DIRS=()
  EVENTS_MIGRATION_TOKEN=""
}

trap 'cleanup_events_migration' EXIT
trap 'cleanup_events_migration; exit 130' INT
trap 'cleanup_events_migration; exit 143' TERM

ensure_trailing_newline() {
  local path="$1"
  [[ -s "$path" ]] || return 0
  if [[ "$(tail -c 1 "$path" | wc -l | tr -d ' ')" == "0" ]]; then
    printf '\n' >> "$path"
  fi
}

journal_ends_with() {
  local journal="$1"
  local suffix="$2"
  local suffix_bytes
  suffix_bytes=$(wc -c < "$suffix" | tr -d ' ')
  (( suffix_bytes == 0 )) && return 0
  [[ -f "$journal" ]] || return 1
  (( $(wc -c < "$journal" | tr -d ' ') >= suffix_bytes )) || return 1
  tail -c "$suffix_bytes" "$journal" | cmp -s - "$suffix"
}

recover_event_cursor_pending() {
  local source="$1"
  local cursor="$2"
  local pending="${cursor}.gc-pending"
  [[ -f "$pending" ]] || return 0
  python3 - "$source" "$cursor" "$pending" <<'PY'
import json
import os
import sys
import tempfile

source, cursor, pending = sys.argv[1:]
payload = json.loads(open(pending, encoding="ascii").read())
stat = os.stat(source)
if payload.get("device") != stat.st_dev or payload.get("inode") != stat.st_ino:
    os.unlink(pending)
    raise SystemExit(0)
value = payload.get("cursor")
if not isinstance(value, int) or isinstance(value, bool) or value < 0:
    raise ValueError("invalid pending event cursor")
fd, temp = tempfile.mkstemp(dir=os.path.dirname(cursor), prefix=f".{os.path.basename(cursor)}.")
try:
    with os.fdopen(fd, "w", encoding="ascii") as handle:
        handle.write(str(value))
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temp, cursor)
    os.unlink(pending)
finally:
    try:
        os.unlink(temp)
    except FileNotFoundError:
        pass
PY
}

publish_event_cursor_pending() {
  local staged="$1"
  local cursor="$2"
  local value="$3"
  python3 - "$staged" "${cursor}.gc-pending" "$value" <<'PY'
import json
import os
import sys
import tempfile

staged, pending, raw_value = sys.argv[1:]
stat = os.stat(staged)
payload = json.dumps(
    {"device": stat.st_dev, "inode": stat.st_ino, "cursor": int(raw_value)},
    separators=(",", ":"),
).encode("ascii")
fd, temp = tempfile.mkstemp(dir=os.path.dirname(pending), prefix=f".{os.path.basename(pending)}.")
try:
    with os.fdopen(fd, "wb") as handle:
        handle.write(payload)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temp, pending)
finally:
    try:
        os.unlink(temp)
    except FileNotFoundError:
        pass
PY
}

publish_events_migration_receipt() {
  local staged="$1"
  local receipt="$2"
  local migration_id="$3"
  local local_events="$4"
  python3 - "$staged" "$receipt" "$migration_id" "$local_events" <<'PY'
import datetime
import json
import os
import sys
import tempfile

staged, receipt, migration_id, local_events = sys.argv[1:]
marker = {
    "ts": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "type": "event_migration_landed",
    "source": "migration",
    "data": {"migration_id": migration_id},
}
with open(staged, "ab+") as handle:
    handle.seek(0, os.SEEK_END)
    if handle.tell() > 0:
        handle.seek(-1, os.SEEK_END)
        if handle.read(1) != b"\n":
            handle.write(b"\n")
    handle.write(json.dumps(marker, separators=(",", ":")).encode("ascii") + b"\n")
    handle.flush()
    os.fsync(handle.fileno())
stat = os.stat(staged)
payload = json.dumps(
    {
        "device": stat.st_dev,
        "inode": stat.st_ino,
        "migration_id": migration_id,
        "segment_size": os.path.getsize(local_events),
    },
    separators=(",", ":"),
).encode("ascii")
fd, temp = tempfile.mkstemp(dir=os.path.dirname(receipt), prefix=f".{os.path.basename(receipt)}.")
try:
    with os.fdopen(fd, "wb") as handle:
        handle.write(payload)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temp, receipt)
finally:
    try:
        os.unlink(temp)
    except FileNotFoundError:
        pass
PY
}

event_migration_id() {
  local local_events="$1"
  python3 - "$local_events" <<'PY'
import hashlib
import os
import sys

path = os.path.abspath(sys.argv[1])
digest = hashlib.sha256()
digest.update(os.fsencode(path))
digest.update(b"\0")
with open(path, "rb") as handle:
    for chunk in iter(lambda: handle.read(1024 * 1024), b""):
        digest.update(chunk)
print(digest.hexdigest())
PY
}

journal_has_event_migration() {
  local source="$1"
  local migration_id="$2"
  python3 - "$source" "$migration_id" <<'PY'
import json
import sys

source, migration_id = sys.argv[1:]
with open(source, "rb") as handle:
    for raw in handle:
        try:
            row = json.loads(raw)
        except (json.JSONDecodeError, UnicodeDecodeError):
            continue
        if (
            isinstance(row, dict)
            and row.get("type") == "event_migration_landed"
            and isinstance(row.get("data"), dict)
            and row["data"].get("migration_id") == migration_id
        ):
            raise SystemExit(0)
raise SystemExit(1)
PY
}

migration_receipt_matches() {
  local source="$1"
  local receipt="$2"
  local migration_id="$3"
  python3 - "$source" "$receipt" "$migration_id" <<'PY'
import json
import os
import sys

source, receipt, migration_id = sys.argv[1:]
try:
    payload = json.loads(open(receipt, encoding="ascii").read())
    stat = os.stat(source)
except (OSError, ValueError):
    raise SystemExit(1)
raise SystemExit(
    0
    if payload.get("device") == stat.st_dev
    and payload.get("inode") == stat.st_ino
    and payload.get("migration_id") == migration_id
    else 1
)
PY
}

migration_receipt_prefix_size() {
  local source="$1"
  local receipt="$2"
  local local_events="$3"
  python3 - "$source" "$receipt" "$local_events" <<'PY'
import hashlib
import json
import os
import sys

source, receipt, local_events = sys.argv[1:]
try:
    payload = json.loads(open(receipt, encoding="ascii").read())
    stat = os.stat(source)
    size = payload["segment_size"]
    expected = payload["migration_id"]
    if (
        payload.get("device") != stat.st_dev
        or payload.get("inode") != stat.st_ino
        or not isinstance(size, int)
        or isinstance(size, bool)
        or size < 0
        or size >= os.path.getsize(local_events)
        or not isinstance(expected, str)
    ):
        raise ValueError
    digest = hashlib.sha256()
    digest.update(os.fsencode(os.path.abspath(local_events)))
    digest.update(b"\0")
    with open(local_events, "rb") as handle:
        remaining = size
        while remaining:
            chunk = handle.read(min(remaining, 1024 * 1024))
            if not chunk:
                raise ValueError
            digest.update(chunk)
            remaining -= len(chunk)
except (KeyError, OSError, ValueError):
    raise SystemExit(1)
if digest.hexdigest() != expected:
    raise SystemExit(1)
print(size)
PY
}

reconcile_status_fanout_cursors() {
  local source="$1"
  local target="$2"
  shift 2
  python3 - "$(dirname "$source")/status-sinks" "$(dirname "$target")/status-sinks" "$@" <<'PY'
import json
import os
import re
import secrets
import sys
import tempfile
from datetime import datetime, timedelta
from pathlib import Path

canonical_dir, local_dir = map(Path, sys.argv[1:3])
migrated_segments = [Path(value) for value in sys.argv[3:]]
canonical_dir.mkdir(parents=True, exist_ok=True)


def timestamp(value):
    if (
        not isinstance(value, str)
        or re.fullmatch(
            r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]{1,6})?(?:Z|\+00:00)",
            value,
        )
        is None
    ):
        raise ValueError
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.utcoffset() != timedelta(0):
        raise ValueError
    return parsed


def read_cursor(path):
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
        ts = payload["ts"]
        count = payload["n"]
        if (
            not isinstance(count, int)
            or isinstance(count, bool)
            or count < 0
        ):
            raise ValueError
        return ts, count, timestamp(ts)
    except (KeyError, OSError, TypeError, UnicodeError, ValueError):
        return None


def atomic_cursor(path, cursor):
    payload = json.dumps({"ts": cursor[0], "n": cursor[1]}, separators=(",", ":"))
    fd, temp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temp, path)
    finally:
        try:
            os.unlink(temp)
        except FileNotFoundError:
            pass


def retire_cursor(local_path, canonical_path):
    temp = local_path.with_name(
        f".{local_path.name}.{os.getpid()}.{secrets.token_hex(8)}"
    )
    try:
        os.symlink(canonical_path.resolve(strict=True), temp)
        os.replace(temp, local_path)
    finally:
        try:
            os.unlink(temp)
        except FileNotFoundError:
            pass


earliest_migrated = None
for segment in migrated_segments:
    try:
        with segment.open(encoding="utf-8") as handle:
            for line in handle:
                try:
                    raw = json.loads(line).get("ts")
                    parsed = timestamp(raw)
                except (AttributeError, TypeError, ValueError):
                    continue
                if earliest_migrated is None or parsed < earliest_migrated[1]:
                    earliest_migrated = (raw, parsed)
    except (OSError, UnicodeError):
        continue

local_names = set()
for local_path in local_dir.glob("*.cursor") if local_dir.is_dir() else ():
    local = read_cursor(local_path)
    if local is None:
        continue
    local_names.add(local_path.name)
    canonical_path = canonical_dir / local_path.name
    try:
        if os.path.samefile(local_path, canonical_path):
            continue
    except OSError:
        pass
    canonical = read_cursor(canonical_path)
    if canonical is None or local[2] < canonical[2]:
        # The merged journal places canonical rows before the local segment.
        # Reset the occurrence index at the earlier timestamp: replay is legal
        # for the at-least-once sink, while skipping a delayed local row is not.
        atomic_cursor(canonical_path, (local[0], 0))
    retire_cursor(local_path, canonical_path)

if earliest_migrated is not None:
    for canonical_path in canonical_dir.glob("*.cursor"):
        if canonical_path.name in local_names:
            continue
        canonical = read_cursor(canonical_path)
        if canonical is None or earliest_migrated[1] <= canonical[2]:
            atomic_cursor(canonical_path, (earliest_migrated[0], 0))
PY
}

reconcile_shared_events_fanout() {
  local source="$1"
  local target="$2"
  local token="$3"
  local source_fanout_dir="$(dirname "$source")/status-sinks"
  local target_fanout_dir="$(dirname "$target")/status-sinks"
  local first_fanout_lock="${source_fanout_dir}/.tick.lock.d"
  local second_fanout_lock="${target_fanout_dir}/.tick.lock.d"
  mkdir -p "$source_fanout_dir" "$target_fanout_dir"
  if [[ "$second_fanout_lock" < "$first_fanout_lock" ]]; then
    local swap_fanout_lock="$first_fanout_lock"
    first_fanout_lock="$second_fanout_lock"
    second_fanout_lock="$swap_fanout_lock"
  fi
  EVENTS_MIGRATION_TOKEN="$token"
  EVENTS_MIGRATION_DIRS=()
  if ! acquire_events_dir "$first_fanout_lock" "$token"; then
    echo "setup-worktree: status fanout reconciliation timed out on $first_fanout_lock" >&2
    return 1
  fi
  EVENTS_MIGRATION_DIRS+=("$first_fanout_lock")
  if [[ "$second_fanout_lock" != "$first_fanout_lock" ]]; then
    if ! acquire_events_dir "$second_fanout_lock" "$token"; then
      cleanup_events_migration
      echo "setup-worktree: status fanout reconciliation timed out on $second_fanout_lock" >&2
      return 1
    fi
    EVENTS_MIGRATION_DIRS+=("$second_fanout_lock")
  fi
  local rc=0
  if verify_events_migration_leases; then
    reconcile_status_fanout_cursors "$source" "$target" || rc=$?
  else
    rc=1
  fi
  cleanup_events_migration
  return "$rc"
}

append_migrated_events() {
  local source="$1"
  local local_events="$2"
  local deduplicate_suffix="${3:-false}"
  local local_cursor="${4:-}"
  local cursor="$(dirname "$source")/.think-offer-cursor"
  local receipt="${local_events}.landed"
  local migration_id
  migration_id=$(event_migration_id "$local_events") || return 1
  recover_event_cursor_pending "$source" "$cursor" || return 1
  if [[ "$deduplicate_suffix" == "true" ]] && journal_has_event_migration "$source" "$migration_id"; then
    return 0
  fi
  if [[ "$deduplicate_suffix" == "true" ]] && migration_receipt_matches "$source" "$receipt" "$migration_id"; then
    return 0
  fi
  local migration_input="$local_events"
  local migration_cursor="$local_cursor"
  local suffix=""
  if [[ "$deduplicate_suffix" == "true" ]]; then
    local landed_size=""
    landed_size=$(migration_receipt_prefix_size "$source" "$receipt" "$local_events" 2>/dev/null || true)
    if [[ "$landed_size" =~ ^[0-9]+$ ]]; then
      suffix=$(mktemp "${source}.migration-tail.XXXXXX") || return 1
      python3 - "$local_events" "$suffix" "$landed_size" <<'PY' || {
import sys

source, destination, offset = sys.argv[1:]
with open(source, "rb") as handle:
    handle.seek(int(offset))
    with open(destination, "wb") as output:
        output.write(handle.read())
PY
        rm -f "$suffix"
        return 1
      }
      migration_input="$suffix"
      if [[ "$local_cursor" =~ ^[0-9]+$ && "$local_cursor" -gt "$landed_size" ]]; then
        migration_cursor="$((local_cursor - landed_size))"
      else
        migration_cursor=0
      fi
    fi
  fi
  if [[ "$deduplicate_suffix" == "true" ]] && journal_ends_with "$source" "$migration_input"; then
    [[ -z "$suffix" ]] || rm -f "$suffix"
    return 0
  fi
  local filtered mapping
  filtered=$(mktemp "${source}.migration.XXXXXX") || {
    [[ -z "$suffix" ]] || rm -f "$suffix"
    return 1
  }
  mapping=$(mktemp "${source}.migration-cursor.XXXXXX") || {
    rm -f "$filtered" "$suffix"
    return 1
  }
  if ! EVENTS_MIGRATION_LOCAL_CURSOR="$migration_cursor" EVENTS_MIGRATION_CURSOR_MAP="$mapping" \
    python3 "$EVENTS_MIGRATION_FILTER" "$source" "$migration_input" > "$filtered"; then
    rm -f "$filtered" "$mapping" "$suffix"
    return 1
  fi
  local rc=0
  if [[ "$deduplicate_suffix" == "true" ]] && journal_ends_with "$source" "$filtered"; then
    rm -f "$filtered" "$mapping" "$suffix"
    return 0
  fi
  local source_size canonical_cursor consumed_cursor
  source_size=$(wc -c < "$source" | tr -d ' ')
  canonical_cursor="$source_size"
  if [[ -f "$cursor" ]]; then
    canonical_cursor=$(tr -d ' \n' < "$cursor" 2>/dev/null)
  fi
  consumed_cursor=$(tr -d ' \n' < "$mapping" 2>/dev/null)
  local cursor_at_end=0
  if [[ "$canonical_cursor" =~ ^[0-9]+$ && "$canonical_cursor" -le "$source_size" ]]; then
    if [[ "$canonical_cursor" -eq "$source_size" ]] || python3 - "$source" "$canonical_cursor" <<'PY'
import sys
import json

source, raw_offset = sys.argv[1:]
with open(source, "rb") as handle:
    handle.seek(int(raw_offset))
    for raw in handle.read().decode("utf-8", "replace").splitlines():
        try:
            row = json.loads(raw)
        except (json.JSONDecodeError, TypeError):
            continue
        if isinstance(row, dict) and row.get("type") == "think_offered":
            raise SystemExit(1)
PY
    then
      cursor_at_end=1
      canonical_cursor="$source_size"
    fi
  fi
  if (( cursor_at_end == 0 )) || [[ ! "$consumed_cursor" =~ ^[0-9]+$ ]]; then
    echo "setup-worktree: refusing events migration while canonical offers are pending" >&2
    rc=1
  fi
  if (( rc == 0 )); then
    local staged
    staged=$(mktemp "${source}.migration-append.XXXXXX") || rc=$?
    if (( rc == 0 )); then
      cp -p "$source" "$staged" || rc=$?
    fi
    if (( rc == 0 )); then
      cat "$filtered" >> "$staged" || rc=$?
    fi
    if (( rc == 0 )); then
      publish_event_cursor_pending "$staged" "$cursor" "$((canonical_cursor + consumed_cursor))" || rc=$?
    fi
    if (( rc == 0 )); then
      publish_events_migration_receipt "$staged" "$receipt" "$migration_id" "$local_events" || rc=$?
    fi
    if (( rc == 0 )) && verify_events_migration_leases; then
      if mv "$staged" "$source"; then
        EVENTS_MIGRATION_PUBLISHED=1
      else
        rc=$?
      fi
    elif (( rc == 0 )); then
      rc=1
    fi
    if (( rc == 0 )) && verify_events_migration_leases; then
      recover_event_cursor_pending "$source" "$cursor" || rc=$?
    elif (( rc == 0 )); then
      rc=1
    fi
    [[ -z "${staged:-}" ]] || rm -f "$staged"
  fi
  rm -f "$filtered" "$mapping" "$suffix"
  return "$rc"
}

wait_for_shell_event_writers() {
  local events_path="$1"
  local active_dir="${events_path}.shell-writers.d"
  local attempts=0
  local entries=()
  while [[ -d "$active_dir" ]]; do
    renew_events_migration_dirs || return 1
    shopt -s nullglob
    entries=("$active_dir"/*)
    shopt -u nullglob
    local entry name pid recorded_identity current_identity
    if [[ -n "${entries[0]:-}" ]]; then
      for entry in "${entries[@]}"; do
        name="$(basename "$entry")"
        pid="${name%%.*}"
        if [[ "$pid" =~ ^[0-9]+$ ]]; then
          recorded_identity=$(cat "$entry/owner" 2>/dev/null || true)
          if [[ -n "$recorded_identity" ]]; then
            current_identity=$(_event_process_identity "$pid")
            if [[ "$current_identity" != "$recorded_identity" ]]; then
              command -p rm -f "$entry/owner" 2>/dev/null || true
              rmdir "$entry" 2>/dev/null || true
            fi
          elif ! kill -0 "$pid" 2>/dev/null; then
            rmdir "$entry" 2>/dev/null || true
          fi
        fi
      done
    fi
    shopt -s nullglob
    entries=("$active_dir"/*)
    shopt -u nullglob
    if [[ -z "${entries[0]:-}" ]]; then
      rmdir "$active_dir" 2>/dev/null || true
      return 0
    fi
    if (( attempts >= 300 )); then
      return 1
    fi
    sleep 0.1
    attempts=$((attempts + 1))
  done
}

# Migrate a worktree-local journal before linking it to the canonical journal.
# The GC markers pause the bounded shell appenders, and the ordinary mutexes
# pause Python, Rust claims, and Journal writers. Locks are acquired in sorted
# path order so two concurrent setup runs cannot deadlock each other.
link_events_journal() {
  local rel=".fno/events.jsonl"
  local source="$CANONICAL/$rel"
  local target="$WORKTREE/$rel"
  local source_cursor="$CANONICAL/.fno/.think-offer-cursor"
  local target_cursor="$WORKTREE/.fno/.think-offer-cursor"
  local token="$(hostname):$$:$(date -u +%s):$RANDOM"
  local pending_backups=()
  local pending_candidate
  local recover_pending=0
  local fresh_target=0
  local source_resolved target_resolved
  EVENTS_MIGRATION_TOKEN="$token"
  EVENTS_MIGRATION_DIRS=()
  EVENTS_MIGRATION_PUBLISHED=0

  mkdir -p "$(dirname "$source")" "$(dirname "$target")"
  : >> "$source" || {
    echo "setup-worktree: cannot create canonical events journal: $source" >&2
    return 1
  }

  shopt -s nullglob
  for pending_candidate in "${target}.pre-share.pending."*; do
    [[ "$pending_candidate" == *.landed ]] || pending_backups+=("$pending_candidate")
  done
  shopt -u nullglob
  source_resolved=$(_resolve_event_symlink "$source") || return 1
  if [[ -L "$target" ]]; then
    target_resolved=$(_resolve_event_symlink "$target") || return 1
    if [[ "$target_resolved" != "$source_resolved" ]]; then
      echo "setup-worktree: refusing to retarget noncanonical events symlink: $target -> $target_resolved" >&2
      return 1
    fi
  fi
  if [[ -L "$target" && ${#pending_backups[@]} -eq 0 ]]; then
    reconcile_shared_events_fanout "$source" "$target" "$token"
    return $?
  fi
  if [[ -L "$target" ]]; then
    recover_pending=1
  fi
  if [[ -e "$target" && ! -f "$target" ]]; then
    echo "setup-worktree: refusing to replace non-file events journal: $target" >&2
    return 1
  fi

  local source_gc="${source}.gc.d"
  local target_gc="${target}.gc.d"
  local source_lock="${source}.lock.d"
  local target_lock="${target}.lock.d"
  local first_gc="$source_gc" second_gc="$target_gc"
  local first_lock="$source_lock" second_lock="$target_lock"
  if [[ "$second_gc" < "$first_gc" ]]; then
    first_gc="$target_gc"
    second_gc="$source_gc"
  fi
  if [[ "$second_lock" < "$first_lock" ]]; then
    first_lock="$target_lock"
    second_lock="$source_lock"
  fi

  if ! acquire_events_dir "$first_gc" "$token"; then
    echo "setup-worktree: events migration timed out on $first_gc" >&2
    return 1
  fi
  EVENTS_MIGRATION_DIRS+=("$first_gc")
  if ! acquire_events_dir "$second_gc" "$token"; then
    cleanup_events_migration
    echo "setup-worktree: events migration timed out on $second_gc" >&2
    return 1
  fi
  EVENTS_MIGRATION_DIRS+=("$second_gc")
  if ! wait_for_shell_event_writers "$source"; then
    cleanup_events_migration
    echo "setup-worktree: events migration timed out on ${source}.shell-writers.d" >&2
    return 1
  fi
  if ! wait_for_shell_event_writers "$target"; then
    cleanup_events_migration
    echo "setup-worktree: events migration timed out on ${target}.shell-writers.d" >&2
    return 1
  fi
  # Pre-rendezvous shells do not register, so retain one bounded rollout grace.
  sleep 0.1
  if ! renew_events_migration_dirs; then
    cleanup_events_migration
    echo "setup-worktree: events migration lost a mutex lease before writer lock" >&2
    return 1
  fi
  if ! acquire_events_dir "$first_lock" "$token"; then
    cleanup_events_migration
    echo "setup-worktree: events migration timed out on $first_lock" >&2
    return 1
  fi
  EVENTS_MIGRATION_DIRS+=("$first_lock")
  if ! acquire_events_dir "$second_lock" "$token"; then
    cleanup_events_migration
    echo "setup-worktree: events migration timed out on $second_lock" >&2
    return 1
  fi
  EVENTS_MIGRATION_DIRS+=("$second_lock")
  local first_cursor_lock="${source_cursor}.lock.d"
  local second_cursor_lock="${target_cursor}.lock.d"
  if [[ -L "$target_cursor" ]]; then
    second_cursor_lock="$(_resolve_event_symlink "$target_cursor").lock.d" || {
      cleanup_events_migration
      return 1
    }
  fi
  if [[ "$second_cursor_lock" < "$first_cursor_lock" ]]; then
    local swap_cursor_lock="$first_cursor_lock"
    first_cursor_lock="$second_cursor_lock"
    second_cursor_lock="$swap_cursor_lock"
  fi
  if ! acquire_events_dir "$first_cursor_lock" "$token"; then
    cleanup_events_migration
    echo "setup-worktree: event cursor migration timed out on $first_cursor_lock" >&2
    return 1
  fi
  EVENTS_MIGRATION_DIRS+=("$first_cursor_lock")
  if [[ "$second_cursor_lock" != "$first_cursor_lock" ]]; then
    if ! acquire_events_dir "$second_cursor_lock" "$token"; then
      cleanup_events_migration
      echo "setup-worktree: event cursor migration timed out on $second_cursor_lock" >&2
      return 1
    fi
    EVENTS_MIGRATION_DIRS+=("$second_cursor_lock")
  fi
  local source_fanout_dir="$(dirname "$source")/status-sinks"
  local target_fanout_dir="$(dirname "$target")/status-sinks"
  mkdir -p "$source_fanout_dir" "$target_fanout_dir"
  local first_fanout_lock="${source_fanout_dir}/.tick.lock.d"
  local second_fanout_lock="${target_fanout_dir}/.tick.lock.d"
  if [[ "$second_fanout_lock" < "$first_fanout_lock" ]]; then
    local swap_fanout_lock="$first_fanout_lock"
    first_fanout_lock="$second_fanout_lock"
    second_fanout_lock="$swap_fanout_lock"
  fi
  if ! acquire_events_dir "$first_fanout_lock" "$token"; then
    cleanup_events_migration
    echo "setup-worktree: status fanout migration timed out on $first_fanout_lock" >&2
    return 1
  fi
  EVENTS_MIGRATION_DIRS+=("$first_fanout_lock")
  if [[ "$second_fanout_lock" != "$first_fanout_lock" ]]; then
    if ! acquire_events_dir "$second_fanout_lock" "$token"; then
      cleanup_events_migration
      echo "setup-worktree: status fanout migration timed out on $second_fanout_lock" >&2
      return 1
    fi
    EVENTS_MIGRATION_DIRS+=("$second_fanout_lock")
  fi

  # Another setup may have completed while this process waited for the locks.
  # Re-read both the link and recovery receipts before choosing a mutation path.
  shopt -s nullglob
  pending_backups=()
  for pending_candidate in "${target}.pre-share.pending."*; do
    [[ "$pending_candidate" == *.landed ]] || pending_backups+=("$pending_candidate")
  done
  shopt -u nullglob
  source_resolved=$(_resolve_event_symlink "$source") || {
    cleanup_events_migration
    return 1
  }
  if [[ -L "$target" ]]; then
    target_resolved=$(_resolve_event_symlink "$target") || {
      cleanup_events_migration
      return 1
    }
    if [[ "$target_resolved" != "$source_resolved" ]]; then
      cleanup_events_migration
      echo "setup-worktree: refusing to retarget noncanonical events symlink: $target -> $target_resolved" >&2
      return 1
    fi
  fi
  if [[ -L "$target" && ${#pending_backups[@]} -eq 0 ]]; then
    local reconcile_rc=0
    if verify_events_migration_leases; then
      reconcile_status_fanout_cursors "$source" "$target" || reconcile_rc=$?
    else
      reconcile_rc=1
    fi
    cleanup_events_migration
    return "$reconcile_rc"
  fi
  if [[ -L "$target" ]]; then
    recover_pending=1
  elif (( ${#pending_backups[@]} > 0 )); then
    recover_pending=1
  elif [[ ! -e "$target" ]]; then
    fresh_target=1
    recover_pending=0
  elif [[ ! -f "$target" ]]; then
    cleanup_events_migration
    echo "setup-worktree: refusing to replace non-file events journal: $target" >&2
    return 1
  else
    recover_pending=0
  fi

  if [[ ! -e "$source_cursor" ]]; then
    (set -C; printf '%s' 0 > "$source_cursor") 2>/dev/null || {
      if [[ ! -e "$source_cursor" ]]; then
        cleanup_events_migration
        echo "setup-worktree: cannot initialize canonical event cursor: $source_cursor" >&2
        return 1
      fi
    }
  fi

  if ! start_events_migration_keepalive; then
    cleanup_events_migration
    echo "setup-worktree: could not start events migration lease renewal" >&2
    return 1
  fi

  local rc=0
  local leases_owned=1
  local -a migrated_segments=()
  local -a completed_segments=()
  local stamp="$(date -u +%Y%m%dT%H%M%SZ).$$"
  local backup="${target}.pre-share.pending.${stamp}"
  local completed_backup="${target}.pre-share.${stamp}"
  verify_events_migration_leases || { rc=1; leases_owned=0; }
  if (( rc == 0 )); then
    ensure_trailing_newline "$source" || rc=$?
  fi
  if (( rc == 0 && fresh_target == 1 )); then
    verify_events_migration_leases || { rc=1; leases_owned=0; }
  fi
  if (( rc == 0 && fresh_target == 1 )); then
    ln -s "$source" "$target" 2>/dev/null || rc=$?
  elif (( recover_pending == 1 )); then
    local pending completed
    if [[ ! -L "$target" ]]; then
      if [[ -f "$target" ]] && verify_events_migration_leases; then
        mv "$target" "$backup" || rc=$?
        if (( rc == 0 )); then
          pending_backups+=("$backup")
        fi
      elif [[ -f "$target" ]]; then
        rc=1
        leases_owned=0
      fi
      if (( rc == 0 )) && verify_events_migration_leases; then
        ln -s "$source" "$target" || rc=$?
      elif (( rc == 0 )); then
        rc=1
        leases_owned=0
      fi
    fi
    for pending in "${pending_backups[@]}"; do
      if (( rc == 0 )); then
        append_migrated_events "$source" "$pending" true "$target_cursor" || rc=$?
        if (( rc == 0 )) && verify_events_migration_leases; then
          ensure_trailing_newline "$source" || rc=$?
        elif (( rc == 0 )); then
          rc=1
          leases_owned=0
        fi
      fi
      completed="${pending/.pre-share.pending./.pre-share.}"
      if (( rc == 0 )) && verify_events_migration_leases; then
        migrated_segments+=("$pending")
        completed_segments+=("$completed")
      elif (( rc == 0 )); then
        rc=1
        leases_owned=0
      fi
    done
  elif (( rc == 0 )); then
    verify_events_migration_leases || { rc=1; leases_owned=0; }
    if (( rc == 0 )); then
      mv "$target" "$backup" || rc=$?
    fi
    if (( rc == 0 )) && verify_events_migration_leases; then
      ln -s "$source" "$target" || rc=$?
    elif (( rc == 0 )); then
      rc=1
      leases_owned=0
    fi
    if (( rc == 0 )) && [[ -s "$backup" ]]; then
      append_migrated_events "$source" "$backup" false "$target_cursor" || rc=$?
      if (( rc == 0 )) && verify_events_migration_leases; then
        ensure_trailing_newline "$source" || rc=$?
      elif (( rc == 0 )); then
        rc=1
        leases_owned=0
      fi
    fi
    if (( rc == 0 )) && ! verify_events_migration_leases; then
      rc=1
      leases_owned=0
    fi
    if (( rc == 0 )); then
      migrated_segments+=("$backup")
      completed_segments+=("$completed_backup")
    fi
  fi

  if (( rc == 0 )) && verify_events_migration_leases; then
    if (( ${#migrated_segments[@]} > 0 )); then
      reconcile_status_fanout_cursors "$source" "$target" "${migrated_segments[@]}" || rc=$?
    else
      reconcile_status_fanout_cursors "$source" "$target" || rc=$?
    fi
  elif (( rc == 0 )); then
    rc=1
    leases_owned=0
  fi

  if (( rc == 0 && ${#migrated_segments[@]} > 0 )); then
    local segment_index
    for (( segment_index=0; segment_index<${#migrated_segments[@]}; segment_index++ )); do
      if ! verify_events_migration_leases; then
        rc=1
        leases_owned=0
        break
      fi
      if ! mv "${migrated_segments[$segment_index]}" "${completed_segments[$segment_index]}"; then
        echo "setup-worktree: events rows landed; pending backup retained for recovery: ${migrated_segments[$segment_index]}" >&2
        rc=1
        break
      fi
      command -p rm -f "${migrated_segments[$segment_index]}.landed"
    done
  fi

  verify_events_migration_leases || { rc=1; leases_owned=0; }
  if ! stop_events_migration_keepalive; then
    rc=1
    leases_owned=0
    echo "setup-worktree: events migration lost a mutex lease" >&2
  fi

  if (( rc != 0 )); then
    if (( leases_owned == 1 && recover_pending == 0 && EVENTS_MIGRATION_PUBLISHED == 0 )); then
      if [[ -L "$target" ]]; then
        rm -f "$target" 2>/dev/null || true
      fi
      if [[ ! -e "$target" && -e "$backup" ]]; then
        mv "$backup" "$target" 2>/dev/null || true
      fi
    fi
    echo "setup-worktree: events migration failed; local journal retained: $target" >&2
  else
    if (( fresh_target == 1 )); then
      echo "setup-worktree: linked fresh worktree events journal" >&2
    elif (( recover_pending == 1 )); then
      echo "setup-worktree: completed pending events journal migration" >&2
    else
      echo "setup-worktree: migrated events journal; backup retained at $completed_backup" >&2
    fi
  fi

  cleanup_events_migration
  return "$rc"
}

# Shared content (Obsidian vault link)
link_dir "internal"

# Shared fno state (project-level, propagates across worktrees)
link_file ".fno/config.toml"
# One journal per repository makes exact-HEAD gate evidence visible across
# isolated reviewer worktrees. Real worktree journals take the migration path
# above instead of link_file's ordinary skip-if-real-file behavior.
events_journal_shared=0
if link_events_journal; then
  events_journal_shared=1
else
  echo "setup-worktree: events journal left worktree-local after migration failure" >&2
fi
if (( events_journal_shared == 1 )) && [[ -L "$WORKTREE/.fno/events.jsonl" ]]; then
  if [[ ! -e "$CANONICAL/.fno/.think-offer-cursor" ]]; then
    (set -C; printf '%s' 0 > "$CANONICAL/.fno/.think-offer-cursor") 2>/dev/null || true
  fi
  link_artifact ".fno/.think-offer-cursor"
fi
# config.local.toml is deliberately NOT linked: it is the one config file kept
# per-worktree, layering the collision-prone keys (post_merge.parking_lot_path,
# project.id) on top of the shared config.toml (x-cbce). Do not add a
# link_file for it here - a link would re-share exactly the keys it exists to
# diverge. Absent by default (= shared behavior); seed one only when a worktree
# needs its own value.
# ledger.json / ledger.md are deliberately NOT linked: paths.ledger_json() is
# pinned GLOBAL (~/.fno/ledger.json), so a project-local copy is a stray fork,
# not a share. The former dual-write was the split-brain that corrupted
# node-level joins; linking it here re-created the stray in canonical AND every
# worktree, and a setup run whose WORKTREE was canonical linked the file to
# itself (an ELOOP that every .exists() probe reads as simply "absent").
# Do not add a link_file for either - tests/test-register-task.sh cB-AC5-FR
# asserts neither the worktree nor the canonical repo grows a stray ledger.
# carveouts.jsonl: a worktree-local carveout (deferred decision / out-of-scope
# bug) must be visible to the canonical retro-triage harvest at merge, so link
# it to canonical alongside the other shared ledgers. Skip-if-missing until the
# first carveout lands.
link_file ".fno/carveouts.jsonl"
# codemap is a regenerated artifact; last-writer-wins is the desired
# behavior so all worktrees see the latest map.
link_artifact ".fno/codemap.md"

# Wake signals (per-project, NOT per-session). Holds filesystem signals
# dropped by the inbox drain that the project's agents read on wake.
# Skip-if-missing so a fresh canonical doesn't error.
#
# Note: the cross-project inbox itself does NOT live under .fno/.
# Each project's inbox is at internal/agents/{project}/inbox.md (reached
# through the canonical internal/ symlink, which is linked separately
# above). Do not add a `.fno/inbox` link here.
link_dir ".fno/wake-signals"

# Consolidated gate-attestation artifacts ONLY. Per-phase artifacts
# (.fno/artifacts/<phase>-<session_id>.md) stay worktree-local on
# purpose: archive-artifacts.sh's session-aware stale sweep iterates
# `$artifacts_dir/*-*.md` at session end and moves any artifact whose
# frontmatter session_id != current_sid into ${plan_dir}/artifacts-archive/.
# If we symlinked the whole artifacts dir to canonical, worktree A's
# completion sweep would move worktree B's ACTIVE per-phase artifacts AND
# every prior consolidated file out from under them - breaking B's gate
# verification and defeating the "artifacts by PR" persistence goal. The
# consolidator (scripts/lib/consolidate-artifacts.sh) writes its retrospective
# files into the `consolidated/` subdir specifically, and the archive sweep's
# glob does not recurse into subdirectories, so symlinking only that subdir
# gives us cross-worktree persistence without crossing the sweep's reach.
# Codex flagged the original whole-dir link as P1 on PR #320 (round 2).
mkdir -p "$WORKTREE/.fno/artifacts"
# Canonical-side consolidated dir: best-effort. When it already exists as a
# symlink (pre-existing canonical state), `mkdir -p` trips ELOOP ("Too many
# levels of symbolic links"). That is benign - the link target is already
# there - but under `set -e` it would abort the WHOLE setup, leaving every
# link below (.claude/skills, .agents, ...) uncreated. Guard it so the rest
# of the linking always runs.
mkdir -p "$CANONICAL/.fno/artifacts/consolidated" 2>/dev/null || true
link_dir ".fno/artifacts/consolidated"

# Shared Claude Code state (autoMemoryDirectory pin, permission allowlist,
# locally-installed agents/commands/skills)
link_file ".claude/settings.local.json"
link_dir ".claude/agents"
link_dir ".claude/commands"
link_dir ".claude/skills"
# Scheduled tasks: the /schedule skill writes cron-like state here. Project
# level so worktrees see the same schedule and the lock prevents two
# worktrees racing on the same write. Skip-if-missing until the first
# schedule entry lands.
link_file ".claude/scheduled_tasks.json"
link_file ".claude/scheduled_tasks.lock"

# Other gitignored .claude/ state that should follow the canonical:
#   - skill-scoping-state.json: which skills are enabled per scope
#   - audit-progress.txt: long-running audit checkpoint
#   - plans/: free-form planning dir used by some skills
# All skip-if-missing so a fresh canonical doesn't error.
link_file ".claude/.skill-scoping-state.json"
link_file ".claude/audit-progress.txt"
link_dir ".claude/plans"

# Local notes (anything matching .claude/*.local.md is gitignored and
# treated as project-scoped scratchpad). Iterate the canonical so new
# files appear automatically without editing this script.
if [[ -d "$CANONICAL/.claude" ]]; then
  shopt -s nullglob
  for src in "$CANONICAL"/.claude/*.local.md; do
    link_file ".claude/$(basename "$src")"
  done
  shopt -u nullglob
fi

# Per-CLI config roots. All four are gitignored at the top level so they
# are safe to symlink wholesale when present. Skip-if-missing so the link
# step is a no-op for CLIs the canonical hasn't onboarded yet.
#   .agents         - provider/agent config (Codex, openclaw, fno)
#   .codex          - Codex CLI project state
#   .codex-plugin   - Codex plugin manifests
#   .gemini         - Gemini CLI project state (settings.json, agents/)
link_dir ".agents"
link_dir ".codex"
link_dir ".codex-plugin"
link_dir ".gemini"

if (( events_journal_shared == 0 )); then
  echo "setup-worktree: linked independent state but events journal is not shared" >&2
  exit 1
fi

echo "setup-worktree: linked shared state from $CANONICAL into $WORKTREE"
