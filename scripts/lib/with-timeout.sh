#!/usr/bin/env bash
# with-timeout.sh - the one wall-clock bound for shelling out to a binary from
# shell. (The Python side has its own in cli/src/fno/context_observation.py,
# which agrees on the contract: 124, TERM then KILL, by process group.)
#
# Replaces six hand-rolled "portable timeout" helpers that all preferred GNU
# coreutils `timeout` and then disagreed on the fallback. Measured against a hung
# child on a coreutils-free PATH, five of the six capped nothing at all: stock
# macOS ships no timeout(1), so the guard was real only where Homebrew was.
#
# Do NOT reintroduce a `timeout`/`gtimeout` preference in front of this. Two
# reachable implementations of one bound is the defect this replaced, and the
# bash path caps every measured case on bash 3.2: 2.04s for a plain hang, 3.05s
# for a child that ignores TERM and has to be escalated.
#
# Every comment below names a failure that was measured, not reasoned about, and
# several were caught only by a test going red after the code looked correct.

# with_timeout SECS CMD [ARGS...]
#
# Runs CMD, killing it after SECS wall-clock. Returns CMD's own exit status, 124
# if the bound fired, or 2 for a malformed SECS. One status for a fired bound
# (GNU timeout's convention) so no caller compares against a signal number that
# depends on whether TERM or the KILL escalation landed; a command killed by an
# unrelated external signal also reads 124, which is the same decision for a
# caller either way. Safe under `OUTPUT=$(with_timeout ...)`, which is how every
# caller uses it.
with_timeout() {
  local secs="$1"; shift

  # Bare integer only. GNU `timeout` took suffixes like `30m`; `sleep` on stock
  # macOS does not, so such a value would exit the sleep instantly and kill at
  # t=0 - a misconfiguration wearing a fired bound's clothes. Reachable via
  # EVAL_SWEEP_STAGE_TIMEOUT, whose sibling knob defaults to `30m`.
  case "$secs" in
    '' | *[!0-9]*)
      printf 'with_timeout: seconds must be a bare integer, got: %s\n' "$secs" >&2
      return 2 ;;
  esac

  # `set -m` spans BOTH launches so each child is its own process-group leader,
  # which is what makes every negative-pid kill below legal: without job control
  # they share this shell's group and `kill -TERM -$pid` would signal the caller.
  # Each `|| kill ... "$pid"` is the fallback for a host where `set -m` did not
  # take, so the group signal finds no such pgid and the direct target is used
  # instead of the bound silently no-opping.
  set -m

  "$@" &
  local pid=$!

  # Three things here are load-bearing:
  #   - the group, not just $pid. Under a command substitution the capture stays
  #     open until every process holding the pipe exits, so killing a shell
  #     wrapper while its children live leaves the caller blocked regardless.
  #     This is what GNU timeout does internally.
  #   - `>/dev/null`. The watchdog inherits that same capture pipe, so without it
  #     a call that succeeds in 33ms still blocks for the full SECS.
  #   - `&&`, not `;`, so a sleep that fails cannot fall through to an immediate
  #     kill that would read as a fired bound at t=0.
  # TERM then KILL, because TERM alone is not a bound: a child that ignores or
  # traps it leaves the `wait` below with no remaining deadline and this function
  # never returns, which is worse than no cap since the caller is stuck rather
  # than slow. A well-behaved child dies on TERM and the watchdog is reaped
  # during the grace second, so that second is never paid on a normal path.
  ( sleep "$secs" && {
      kill -TERM -"$pid" || kill -TERM "$pid"
      sleep 1
      kill -KILL -"$pid" || kill -KILL "$pid"
    } ) >/dev/null 2>&1 &
  local watchdog=$!

  set +m

  # `|| rc=$?` rather than `; rc=$?`: one caller is a `set -e` gate script, where
  # a bare failing `wait` would take the shell down before rc is read.
  local rc=0
  wait "$pid" 2>/dev/null || rc=$?

  case $rc in
    143 | 137 )
      rc=124
      # `wait` returned the moment the DIRECT child died, which cancels the
      # watchdog below and the escalation still pending inside it. A group member
      # that outlived the TERM while holding the capture pipe would keep
      # `OUTPUT=$(...)` open forever, so finish the group off here. This line is
      # the actual guarantee against that hang; the watchdog's own group kill
      # above is belt-and-braces, and dropping it changes no observable behavior.
      #
      # Conditional on purpose. Signalling a group after a call we did NOT stop
      # would reach whatever the command deliberately left running in it. No
      # caller in this tree is affected either way (every intentional spawn
      # detaches with setsid), but a shared helper should not signal a group on a
      # path where its bound never fired.
      kill -KILL -"$pid" 2>/dev/null || true ;;
  esac

  # By group, so the watchdog's `sleep` dies with it rather than reparenting to
  # pid 1 - the orphan incident scripts/lib/eval-sweep-throttle.sh was written to
  # stop, and doing it this way drops that helper's `pkill -P` dependency, which
  # it notes is absent on a minimal image.
  kill -TERM -"$watchdog" 2>/dev/null || kill -TERM "$watchdog" 2>/dev/null || true

  # Reap it too. An unwaited job that died by signal makes bash announce
  # "Terminated: 15 ( sleep ... )" at the next command boundary on the CALLER's
  # stderr: session noise for an injection hook, log noise for the parity gate.
  wait "$watchdog" 2>/dev/null || true
  return $rc
}
