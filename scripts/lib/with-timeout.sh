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
# Runs CMD, killing it after SECS wall-clock. Returns CMD's own exit status, or
# 124 when the bound fired - one value, following GNU timeout's convention, so a
# caller never compares against a signal number that depends on whether TERM or
# the KILL escalation happened to land. Returns 2 for a malformed SECS.
# A command killed by an unrelated external TERM/KILL also reports 124; from a
# caller's side it was terminated rather than completed, which is the same
# decision either way.
# Safe under `OUTPUT=$(with_timeout ...)`, which is how every caller uses it.
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
  #
  # TERM, then KILL one second later. TERM alone is not a bound: a child that
  # ignores or traps it leaves `wait` below with no remaining deadline and
  # with_timeout never returns, which is worse than having no cap at all because
  # the caller is now stuck rather than merely slow. Verified: a stub that traps
  # TERM and loops survived a 2s bound indefinitely before this escalation.
  # A well-behaved child exits on TERM and the watchdog is reaped during the
  # grace sleep, so the extra second is never actually paid.
  #
  # Each `|| kill ... "$pid"` is a fallback for a host where `set -m` did not
  # take effect, so the group signal finds no such pgid: the direct child still
  # gets signalled rather than the bound silently no-opping.
  ( sleep "$secs" && {
      kill -TERM -"$pid" || kill -TERM "$pid"
      sleep 1
      kill -KILL -"$pid" || kill -KILL "$pid"
    } ) >/dev/null 2>&1 &
  local watchdog=$!

  set +m

  # `|| rc=$?` rather than `; rc=$?`: callers include a `set -e` gate script, and
  # a bare failing `wait` would take the whole shell down before rc is read.
  local rc=0
  wait "$pid" 2>/dev/null || rc=$?

  # A terminated child collapses to ONE status. 143 is the TERM path and 137 the
  # KILL escalation; which one lands is an implementation detail of this file and
  # must not become a condition in a caller.
  case $rc in
    143 | 137 )
      rc=124
      # `wait` returns the moment the DIRECT child dies, which cancels the
      # watchdog below and with it the pending escalation. A group member that
      # outlived the TERM while holding the caller's capture pipe would then keep
      # `OUTPUT=$(...)` open forever - an unbounded hang, worse than the slow
      # path this helper replaced. So finish the group off here, where we already
      # know our bound is what stopped the command.
      #
      # Only on this path. An unconditional group kill would also reap a
      # daemon the command legitimately started (`fno mux ls` autostarts one),
      # turning a successful call into a side effect.
      kill -KILL -"$pid" 2>/dev/null || true ;;
  esac

  # Group-kill the watchdog so its `sleep` dies with it. Killing the subshell
  # alone reparents that sleep to pid 1, which is the orphan-process incident
  # scripts/lib/eval-sweep-throttle.sh was written to stop. Doing it by group
  # also drops that helper's `pkill -P` dependency, which it notes is absent on
  # a minimal image.
  #
  # Same single-pid fallback as the child kills above, and for the same reason:
  # without it, a host where `set -m` did not take effect leaves the watchdog and
  # its sleep alive, and the `wait` below then blocks for the FULL bound on an
  # otherwise fast call.
  kill -TERM -"$watchdog" 2>/dev/null || kill -TERM "$watchdog" 2>/dev/null || true

  # Reap it as well. An unwaited job that died by signal makes bash print
  # "Terminated: 15 ( sleep ... )" at the next command boundary, on the CALLER's
  # stderr - which for an injection hook means noise in the session and for the
  # parity gate means noise in a captured error log.
  wait "$watchdog" 2>/dev/null || true
  return $rc
}
