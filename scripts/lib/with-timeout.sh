#!/usr/bin/env bash
# with-timeout.sh - the one wall-clock bound for shelling out to a binary.
#
# Replaces six hand-rolled "portable timeout" helpers that all preferred GNU
# coreutils `timeout` and then disagreed on the fallback. Measured against a hung
# child on a coreutils-free PATH, five of the six capped nothing at all: stock
# macOS ships no timeout(1), so the guard was real only where Homebrew was.
#
# Do NOT reintroduce a `timeout`/`gtimeout` preference in front of this. Two
# reachable implementations of one bound is the defect this replaced, and the
# bash path caps the hardest measured case in ~2.05s on bash 3.2.

# with_timeout SECS CMD [ARGS...]
#
# Runs CMD, killing it after SECS wall-clock. Returns CMD's exit status, or its
# signal status (143) when the bound fires. Safe under `OUTPUT=$(with_timeout
# ...)`, which is how every caller uses it.
with_timeout() {
  local secs="$1"; shift

  # SECS must be a bare integer. GNU `timeout` accepted suffixes like `30m` and
  # `sleep` on stock macOS does not, so an operator-supplied bound in that form
  # (EVAL_SWEEP_STAGE_TIMEOUT, whose sibling knob defaults to `30m`) would make
  # the sleep exit instantly and kill the command at t=0 - a bound that looks
  # like a timeout but is really a misconfiguration. Refuse instead.
  case "$secs" in
    '' | *[!0-9]*)
      printf 'with_timeout: seconds must be a bare integer, got: %s\n' "$secs" >&2
      return 2 ;;
  esac

  # `set -m` spans BOTH launches, so each becomes its own process-group leader.
  # That is what makes the negative-pid kills below legal: without job control
  # both children share this shell's group and `kill -TERM -$pid` would signal
  # the caller itself.
  set -m

  "$@" &
  local pid=$!

  # Signalling the whole group, not just $pid, is the part that actually works.
  # Under a command substitution the capture does not close until every process
  # holding the pipe exits, so killing a shell wrapper while its children live
  # leaves the caller blocked anyway. Group-signalling is what GNU timeout does
  # internally and why the coreutils path was the only one that ever capped.
  #
  # The >/dev/null is not cosmetic: the watchdog inherits that same capture
  # pipe, so without it a call that succeeds in 53ms still blocks for the full
  # SECS waiting on the sleep to release the pipe.
  # `&&`, not `;`: a sleep that fails for any reason must not fall through to an
  # immediate kill, which would read as a fired bound at t=0.
  # The `|| kill -TERM "$pid"` is a belt-and-braces fallback for a host where
  # `set -m` did not take effect, so the group signal finds no such pgid: the
  # direct child still gets signalled rather than the bound silently no-opping.
  ( sleep "$secs" && { kill -TERM -"$pid" || kill -TERM "$pid"; } ) >/dev/null 2>&1 &
  local watchdog=$!

  set +m

  # `|| rc=$?` rather than `; rc=$?`: callers include a `set -e` gate script, and
  # a bare failing `wait` would take the whole shell down before rc is read.
  local rc=0
  wait "$pid" 2>/dev/null || rc=$?

  # Group-kill the watchdog so its `sleep` dies with it. Killing the subshell
  # alone reparents that sleep to pid 1, which is the orphan-process incident
  # scripts/lib/eval-sweep-throttle.sh was written to stop. Doing it by group
  # also drops that helper's `pkill -P` dependency, which it notes is absent on
  # a minimal image.
  kill -TERM -"$watchdog" 2>/dev/null || true

  # Reap it as well. An unwaited job that died by signal makes bash print
  # "Terminated: 15 ( sleep ... )" at the next command boundary, on the CALLER's
  # stderr - which for an injection hook means noise in the session and for the
  # parity gate means noise in a captured error log.
  wait "$watchdog" 2>/dev/null || true
  return $rc
}
