#!/usr/bin/env bash
# corrections-lock.sh - shared locking helper for corrections.log writers.
#
# Source this from any writer that appends to ~/.claude/corrections.log.
# Provides corrections_lock_append() which acquires an exclusive lock,
# appends the given line, and releases.
#
# macOS lacks flock(1), so we use a mkdir-mutex pattern (POSIX atomic) with
# PID-stamped stale-lock recovery. Linux uses the same approach for
# cross-platform consistency.
#
# Usage:
#   source "$(dirname "${BASH_SOURCE[0]}")/lib/corrections-lock.sh"
#   corrections_lock_append "$LOG_PATH" "$LINE"

# shellcheck shell=bash

# Pin a UTF-8 locale so ${#text} counts codepoints rather than bytes.
# Without this, launchd/cron-spawned writers run under whatever locale
# the daemon inherits (often C/POSIX), and the 200-char truncation in
# corrections_escape_details can split a multibyte UTF-8 character.
if [[ -z "${LC_ALL:-}" ]]; then
  export LC_ALL=en_US.UTF-8 2>/dev/null || export LC_ALL=C.UTF-8 2>/dev/null || true
fi

# Stale-lock reap threshold (seconds). A lock dir whose holder PID is no
# longer alive AND whose mtime is older than this is reclaimable.
_CORRECTIONS_STALE_LOCK_SECONDS="${CORRECTIONS_STALE_LOCK_SECONDS:-60}"

_corrections_pid_alive() {
  kill -0 "$1" 2>/dev/null
}

# Attempt to reap a stale lock_dir. Returns 0 if reaped (caller can retry
# mkdir), 1 if the lock is fresh enough or held by a live process.
_corrections_reap_stale_lock() {
  local lock_dir="$1"
  [[ -d "$lock_dir" ]] || return 1

  # Read holder pid stamped at acquire time (best-effort).
  local holder_pid=""
  if [[ -f "$lock_dir/pid" ]]; then
    holder_pid=$(cat "$lock_dir/pid" 2>/dev/null || echo "")
  fi

  if [[ -n "$holder_pid" ]] && _corrections_pid_alive "$holder_pid"; then
    return 1  # holder still running
  fi

  # Holder is dead (or pid was never stamped). Also require mtime to be
  # older than the stale threshold so we never reap a lock the holder
  # just acquired and is about to release.
  local lock_age=0
  if [[ "$(uname)" == "Darwin" ]]; then
    local now mt
    now=$(date -u +%s)
    mt=$(stat -f "%m" "$lock_dir" 2>/dev/null || echo "$now")
    lock_age=$((now - mt))
  else
    local now mt
    now=$(date -u +%s)
    mt=$(stat -c "%Y" "$lock_dir" 2>/dev/null || echo "$now")
    lock_age=$((now - mt))
  fi

  if [[ "$lock_age" -lt "$_CORRECTIONS_STALE_LOCK_SECONDS" ]]; then
    return 1
  fi

  rm -rf "$lock_dir" 2>/dev/null || return 1
  echo "corrections-lock: reaped stale lock dir $lock_dir (holder pid=$holder_pid, age=${lock_age}s)" >&2
  return 0
}

# Wait up to N seconds (default 1) to acquire the lock by repeatedly trying
# mkdir on the lock directory. Returns 0 on acquire, 1 on timeout.
_corrections_acquire_lock() {
  local lock_dir="$1"
  local timeout="${2:-1}"
  local elapsed=0
  local sleep_step="0.05"

  while ! mkdir "$lock_dir" 2>/dev/null; do
    # Single stale-lock recovery attempt per acquire call.
    if _corrections_reap_stale_lock "$lock_dir"; then
      continue
    fi
    elapsed=$(awk -v e="$elapsed" -v s="$sleep_step" 'BEGIN{ printf "%.2f", e+s }')
    if awk -v e="$elapsed" -v t="$timeout" 'BEGIN{ exit !(e >= t) }'; then
      return 1
    fi
    sleep "$sleep_step"
  done
  # Stamp the lock with the holder's PID so a future stale-reap can detect
  # crash-leaked locks. Best-effort; if write fails the reaper falls back
  # to mtime alone.
  printf '%s\n' "$$" > "$lock_dir/pid" 2>/dev/null || true
  return 0
}

_corrections_release_lock() {
  local lock_dir="$1"
  rm -rf "$lock_dir" 2>/dev/null || true
}

# corrections_lock_append <log_path> <line>
# Appends a single line to log_path while holding an exclusive lock.
# Adds a trailing newline. Exits non-zero if the lock cannot be acquired.
# Releases the lock under signal-trap so SIGTERM/SIGINT mid-append still
# clean up; the trap is local-scoped so callers' traps are unaffected.
corrections_lock_append() {
  local log_path="$1"
  local line="$2"
  local lock_dir="${log_path}.lock.d"
  local timeout="${CORRECTIONS_LOCK_TIMEOUT:-1}"

  if [[ -z "$log_path" || -z "$line" ]]; then
    echo "corrections_lock_append: missing log_path or line" >&2
    return 2
  fi

  if ! _corrections_acquire_lock "$lock_dir" "$timeout"; then
    echo "corrections_lock_append: timeout acquiring $lock_dir after ${timeout}s" >&2
    return 1
  fi

  # Trap signals so a SIGTERM from launchd or a SIGHUP from terminal-close
  # mid-append still releases the lock. The trap is restored on normal
  # exit so callers' traps are not clobbered.
  local _prev_int_trap _prev_term_trap
  _prev_int_trap=$(trap -p INT 2>/dev/null || true)
  _prev_term_trap=$(trap -p TERM 2>/dev/null || true)
  # shellcheck disable=SC2064  # we want $lock_dir to expand NOW, not when signalled
  trap "_corrections_release_lock '$lock_dir'" INT TERM

  local rc=0
  printf '%s\n' "$line" >> "$log_path" || rc=$?
  _corrections_release_lock "$lock_dir"

  # Restore prior traps (or clear if none were set).
  if [[ -n "$_prev_int_trap" ]]; then
    eval "$_prev_int_trap"
  else
    trap - INT
  fi
  if [[ -n "$_prev_term_trap" ]]; then
    eval "$_prev_term_trap"
  else
    trap - TERM
  fi

  return "$rc"
}

# corrections_log_path - resolve the log path with environment override.
corrections_log_path() {
  local claude_dir="${CLAUDE_DIR_OVERRIDE:-$HOME/.claude}"
  printf '%s\n' "$claude_dir/corrections.log"
}

# corrections_escape_details <text> - sanitise free-text for the DETAILS field.
# 1. Strip ASCII control chars (0x00-0x1F except space) and DEL (0x7F).
#    A commit subject containing terminal escape sequences would otherwise
#    flow into corrections.log and through to the LLM packet as a
#    prompt-injection vector.
# 2. Replace literal pipes with `\|` (pipes are the line-field separator).
# 3. Replace newlines with " ; " (lines are single-line by contract).
# 4. Truncate to 200 codepoints with a trailing "..." marker.
corrections_escape_details() {
  local text="$1"
  # Strip control chars except tab and space. Use python so the byte-by-byte
  # filter is unambiguous regardless of shell locale.
  text=$(printf '%s' "$text" | python3 -c '
import sys
raw = sys.stdin.read()
# Keep tab (0x09) and printable ASCII + UTF-8 multibyte. Drop 0x00-0x08,
# 0x0B-0x1F, and 0x7F. Newlines (0x0A) are handled by the caller below.
out = "".join(c for c in raw if c == "\t" or c == "\n" or (ord(c) >= 0x20 and ord(c) != 0x7F))
sys.stdout.write(out)
' 2>/dev/null || printf '%s' "$text")
  # Replace literal pipes with \| and newlines with " ; "
  text="${text//|/\\|}"
  text="${text//$'\n'/ ; }"
  if [[ "${#text}" -gt 200 ]]; then
    text="${text:0:197}..."
  fi
  printf '%s' "$text"
}

# corrections_validate_severity <severity> - exit 0 if valid, 2 if not.
corrections_validate_severity() {
  case "$1" in
    S0|S1|S2) return 0 ;;
    *) echo "unknown severity: $1 (expected S0|S1|S2)" >&2; return 2 ;;
  esac
}

# corrections_validate_source <source> - exit 0 if valid (non-empty, no pipes), 2 if not.
# Centralises the SOURCE field invariant so every writer can call it
# without re-implementing the check.
corrections_validate_source() {
  if [[ -z "$1" ]]; then
    echo "source: must be non-empty" >&2
    return 2
  fi
  if [[ "$1" == *"|"* ]]; then
    echo "source: cannot contain '|' character" >&2
    return 2
  fi
  return 0
}

# corrections_build_line <source> <severity> <location> <details>
# Canonical factory for corrections.log lines. All writers SHOULD route
# through this rather than assembling the line themselves, so the
# format invariant lives in exactly one place.
# Validates source + severity. Escapes details. Emits the assembled line
# on stdout with no trailing newline (caller appends).
# Returns 2 on validation failure.
corrections_build_line() {
  local source_field="$1" severity="$2" location="$3" details="$4"
  corrections_validate_source "$source_field" || return 2
  corrections_validate_severity "$severity" || return 2
  [[ -z "$location" ]] && location="-"
  local safe_details ts
  safe_details="$(corrections_escape_details "$details")"
  ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  printf '%s | %s | %s | %s | %s' "$ts" "$severity" "$source_field" "$location" "$safe_details"
}
